//! bottle-cli's caching CA: what lets the host side read a bottle's HTTPS to the hosts whose
//! downloads it keeps (see cache.rs), as bottled's egress CA does for a credential's (bottle/ca.py).
//!
//! Each machine (and builds, together) has its own, constrained to its cached hosts by a name
//! constraint, so a certificate it signs is good for nothing else; the bottle trusts it, beside
//! bottled's, once bottle has installed it. Made when it's first asked for (`bottle-cli ca
//! certificate`), kept in $BOTTLE_HOME/krun/ca/NAME/HASH/, HASH being of its hosts: other hosts
//! make another CA, and the old one goes. Its key never leaves bottle-cli. A host's certificate is
//! minted from it when a connection first needs one, with one leaf key per CA, as bottled does.
//!
//! Certificates start an hour before they're made: a guest's clock a little behind the host's
//! would otherwise find one not yet valid.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, SanType, SerialNumber,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

const CA_DAYS: u64 = 3650;
/// The most a publicly trusted certificate may have; some clients refuse longer.
const LEAF_DAYS: u64 = 397;
const BACKDATED: Duration = Duration::from_secs(3600);
/// What a DNS name constraint is set to when no DNS name is permitted: nothing is under it.
const NO_DNS_NAME: &str = "invalid";

/// Where caching CAs are kept: $BOTTLE_HOME/krun/ca.
fn root() -> PathBuf {
    let home = std::env::var_os("BOTTLE_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".bottle"),
        PathBuf::from,
    );
    home.join("krun/ca")
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// `name`'s directory of CAs: a machine's name, or "builds".
fn named(name: &str) -> io::Result<PathBuf> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !ok {
        return Err(other(format!("{name:?} can't name a CA")));
    }
    Ok(root().join(name))
}

/// The host patterns a CA may be constrained to, sorted and checked: a host name, an IP address,
/// or `*.` and a host name for the names below it.
fn patterns(hosts: &[String]) -> io::Result<Vec<String>> {
    let patterns: BTreeSet<String> = hosts.iter().map(|h| h.to_lowercase()).collect();
    for pattern in &patterns {
        if pattern.parse::<IpAddr>().is_err() && !is_dns_name(pattern.trim_start_matches("*.")) {
            return Err(other(format!(
                "{pattern:?} can't be a cached host: use a host name, an IP address, or *. and a host name"
            )));
        }
    }
    Ok(patterns.into_iter().collect())
}

fn is_dns_name(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && !host.split('.').any(str::is_empty)
}

fn hash(patterns: &[String]) -> String {
    let digest = Sha256::digest(patterns.join("\n").as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// `name`'s CA certificate, as PEM, constrained to `hosts`; None when there are none. Made if
/// missing; `name`'s CAs for other hosts are removed.
pub fn certificate(name: &str, hosts: &[String]) -> io::Result<Option<String>> {
    let patterns = patterns(hosts)?;
    let keep = if patterns.is_empty() {
        None
    } else {
        Some(ensure(name, &patterns)?)
    };
    if let Ok(entries) = fs::read_dir(named(name)?) {
        for entry in entries.flatten() {
            let path = entry.path();
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            if Some(&path) != keep.as_ref() && !hidden {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
    keep.map(|dir| fs::read_to_string(dir.join("ca.crt")))
        .transpose()
}

/// Removes `name`'s CAs, when its machine is deleted.
pub fn forget(name: &str) -> io::Result<()> {
    match fs::remove_dir_all(named(name)?) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The directory of `name`'s CA for `patterns`, made if missing: in a hidden directory beside it,
/// renamed into place, so two processes making it at once agree on one.
fn ensure(name: &str, patterns: &[String]) -> io::Result<PathBuf> {
    let dir = named(name)?.join(hash(patterns));
    if dir.join("ca.crt").is_file() {
        return Ok(dir);
    }
    let parent = dir.parent().unwrap();
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".ca-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir(&staging)?;
    let made = (|| -> io::Result<()> {
        let key = KeyPair::generate().map_err(io::Error::other)?;
        let params = ca_params(name, patterns)?;
        let cert = params.self_signed(&key).map_err(io::Error::other)?;
        let leaf = KeyPair::generate().map_err(io::Error::other)?;
        write_private(&staging.join("ca.key"), &key.serialize_pem())?;
        write_private(&staging.join("leaf.key"), &leaf.serialize_pem())?;
        fs::write(staging.join("ca.crt"), cert.pem())?;
        fs::write(staging.join("hosts"), patterns.join("\n") + "\n")?;
        Ok(())
    })();
    if let Err(e) = made {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    if fs::rename(&staging, &dir).is_err() {
        // Made by another process first: its CA it is.
        let _ = fs::remove_dir_all(&staging);
        if !dir.join("ca.crt").is_file() {
            return Err(other(format!("couldn't make the CA at {}", dir.display())));
        }
    }
    Ok(dir)
}

fn write_private(path: &Path, content: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(content.as_bytes())
}

/// A CA's parameters: the same each time for the same name and hosts, so a CA read back signs
/// as the one made.
fn ca_params(name: &str, patterns: &[String]) -> io::Result<CertificateParams> {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("bottle caching CA for {name}"));
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let addresses: Vec<IpAddr> = patterns.iter().filter_map(|p| p.parse().ok()).collect();
    let mut permitted: Vec<GeneralSubtree> = patterns
        .iter()
        .filter(|p| p.parse::<IpAddr>().is_err())
        .map(|p| {
            GeneralSubtree::DnsName(match p.strip_prefix("*.") {
                Some(below) => format!(".{below}"),
                None => p.clone(),
            })
        })
        .collect();
    if permitted.is_empty() {
        permitted.push(GeneralSubtree::DnsName(NO_DNS_NAME.into()));
    }
    let mut excluded = Vec::new();
    let v4: Vec<_> = addresses.iter().filter(|a| a.is_ipv4()).collect();
    let v6: Vec<_> = addresses.iter().filter(|a| a.is_ipv6()).collect();
    for address in v4.iter().chain(&v6) {
        permitted.push(GeneralSubtree::IpAddress(match address {
            IpAddr::V4(a) => CidrSubnet::V4(a.octets(), [255; 4]),
            IpAddr::V6(a) => CidrSubnet::V6(a.octets(), [255; 16]),
        }));
    }
    if v4.is_empty() {
        excluded.push(GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])));
    }
    if v6.is_empty() {
        excluded.push(GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])));
    }
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: permitted,
        excluded_subtrees: excluded,
    });
    let now = SystemTime::now();
    params.not_before = when(now - BACKDATED)?;
    params.not_after = when(now + Duration::from_secs(CA_DAYS * 86400))?;
    Ok(params)
}

fn when(time: SystemTime) -> io::Result<time::OffsetDateTime> {
    let seconds = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    time::OffsetDateTime::from_unix_timestamp(seconds as i64).map_err(io::Error::other)
}

/// A CA to mint certificates from: `name`'s, as `certificate` last made it.
pub struct Authority {
    patterns: Vec<String>,
    issuer: Issuer<'static, KeyPair>,
    ca: CertificateDer<'static>,
    leaf: KeyPair,
}

impl Authority {
    /// `name`'s CA, if it has one.
    pub fn load(name: &str) -> io::Result<Option<Authority>> {
        let Ok(entries) = fs::read_dir(named(name)?) else {
            return Ok(None);
        };
        let Some(dir) = entries
            .flatten()
            .map(|e| e.path())
            .find(|p| p.join("ca.crt").is_file())
        else {
            return Ok(None);
        };
        let read = |file: &str| {
            fs::read_to_string(dir.join(file))
                .map_err(|e| other(format!("{}: {e}", dir.join(file).display())))
        };
        let patterns: Vec<String> = read("hosts")?.lines().map(String::from).collect();
        let key = KeyPair::from_pem(&read("ca.key")?).map_err(io::Error::other)?;
        let leaf = KeyPair::from_pem(&read("leaf.key")?).map_err(io::Error::other)?;
        let ca = rustls::pki_types::pem::PemObject::from_pem_slice(read("ca.crt")?.as_bytes())
            .map_err(|e| other(format!("{}: {e}", dir.join("ca.crt").display())))?;
        Ok(Some(Authority {
            issuer: Issuer::new(ca_params(name, &patterns)?, key),
            patterns,
            ca,
            leaf,
        }))
    }

    /// Whether `host` is one of the CA's hosts.
    pub fn permits(&self, host: &str) -> bool {
        let host = host.to_lowercase();
        self.patterns.iter().any(|p| {
            *p == host
                || p.strip_prefix('*')
                    .is_some_and(|below| host.ends_with(below) && host.len() > below.len())
        })
    }

    /// A certificate for `host` and its key: the chain a client is sent, the leaf then the CA.
    pub fn mint(
        &self,
        host: &str,
    ) -> io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        let host = host.to_lowercase();
        if !self.permits(&host) {
            return Err(other(format!("the caching CA isn't for {host}")));
        }
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::OrganizationName, "bottle caching");
        params.subject_alt_names = vec![match host.parse::<IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(host.clone().try_into().map_err(io::Error::other)?),
        }];
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let mut serial = [0u8; 16];
        fs::File::open("/dev/urandom")
            .and_then(|mut f| io::Read::read_exact(&mut f, &mut serial))?;
        serial[0] &= 0x7f; // positive
        params.serial_number = Some(SerialNumber::from(serial.to_vec()));
        let now = SystemTime::now();
        params.not_before = when(now - BACKDATED)?;
        params.not_after = when(now + Duration::from_secs(LEAF_DAYS * 86400))?;
        let cert = params
            .signed_by(&self.leaf, &self.issuer)
            .map_err(io::Error::other)?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.leaf.serialize_der()));
        Ok((vec![cert.der().clone(), self.ca.clone()], key))
    }
}

/// Runs `test` with $BOTTLE_HOME a directory of its own (the tests that set it run one at a
/// time, under a lock).
#[cfg(test)]
pub(crate) fn in_home(test: impl FnOnce(&Path)) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("bottle-ca-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    // SAFETY: the tests that read BOTTLE_HOME hold LOCK.
    unsafe { std::env::set_var("BOTTLE_HOME", &home) };
    test(&home);
    let _ = fs::remove_dir_all(&home);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn a_ca_is_made_once_for_its_hosts_and_again_for_others() {
        in_home(|home| {
            let first = certificate("m1", &hosts(&["repo.maven.example", "*.cdn.example"]))
                .unwrap()
                .unwrap();
            assert!(first.starts_with("-----BEGIN CERTIFICATE-----"));
            let again = certificate("m1", &hosts(&["*.CDN.example", "repo.maven.example"]))
                .unwrap()
                .unwrap();
            assert_eq!(first, again); // the same hosts, however written: the same CA
            let other = certificate("m1", &hosts(&["deb.example"]))
                .unwrap()
                .unwrap();
            assert_ne!(first, other);
            assert_eq!(fs::read_dir(home.join("krun/ca/m1")).unwrap().count(), 1); // the old one gone
            assert_eq!(certificate("m1", &[]).unwrap(), None);
            assert_eq!(fs::read_dir(home.join("krun/ca/m1")).unwrap().count(), 0);
            let key = fs::read_dir(home.join("krun/ca/m1")).unwrap().count();
            assert_eq!(key, 0);
            assert!(certificate("m1", &hosts(&["bad host"])).is_err());
            assert!(certificate("../x", &hosts(&["a.example"])).is_err());
        });
    }

    #[test]
    fn its_key_is_its_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        in_home(|home| {
            certificate("m1", &hosts(&["a.example"])).unwrap();
            let dir = fs::read_dir(home.join("krun/ca/m1"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            for key in ["ca.key", "leaf.key"] {
                let mode = fs::metadata(dir.join(key)).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{key}");
            }
        });
    }

    #[test]
    fn a_hosts_certificate_is_minted_from_it_and_a_tls_client_trusting_it_accepts_it() {
        in_home(|_| {
            let pem = certificate("m1", &hosts(&["*.maven.example", "deb.example"]))
                .unwrap()
                .unwrap();
            let authority = Authority::load("m1").unwrap().unwrap();
            assert!(authority.permits("repo.maven.example"));
            assert!(authority.permits("DEB.example"));
            assert!(!authority.permits("maven.example"));
            assert!(!authority.permits("evil.example"));
            assert!(authority.mint("evil.example").is_err());
            let (chain, key) = authority.mint("repo.maven.example").unwrap();
            assert_eq!(chain.len(), 2);

            // A TLS handshake, in memory: a client trusting only the CA, the server presenting
            // the minted certificate.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let server = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .unwrap();
            let mut roots = rustls::RootCertStore::empty();
            let ca: CertificateDer =
                rustls::pki_types::pem::PemObject::from_pem_slice(pem.as_bytes()).unwrap();
            roots.add(ca).unwrap();
            let client = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let handshake = |name: &str| {
                let mut c = rustls::ClientConnection::new(
                    std::sync::Arc::new(client.clone()),
                    name.to_string().try_into().unwrap(),
                )
                .unwrap();
                let mut s =
                    rustls::ServerConnection::new(std::sync::Arc::new(server.clone())).unwrap();
                for _ in 0..10 {
                    let mut buf = Vec::new();
                    c.write_tls(&mut buf).unwrap();
                    s.read_tls(&mut &buf[..]).unwrap();
                    if s.process_new_packets().is_err() {
                        return false;
                    }
                    let mut buf = Vec::new();
                    s.write_tls(&mut buf).unwrap();
                    c.read_tls(&mut &buf[..]).unwrap();
                    if c.process_new_packets().is_err() {
                        return false;
                    }
                    if !c.is_handshaking() && !s.is_handshaking() {
                        return true;
                    }
                }
                false
            };
            assert!(handshake("repo.maven.example"));
            // Its certificate is for its host alone.
            assert!(!handshake("other.maven.example"));
        });
    }
}
