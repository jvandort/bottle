"""Egress CAs: what lets the egress proxy read a bottle's HTTPS to the hosts a credential names.

A credential is attached as a request header, and a header can only be
attached to a request the proxy can read. A tool that talks HTTPS through
CONNECT sends the proxy an opaque tunnel, and many tools only talk HTTPS (gh,
and Claude Code for everything but inference). So for those hosts the proxy
answers the CONNECT itself: it completes the bottle's TLS handshake with a
certificate for the host, signed by the bottle's CA, reads the request,
attaches the credential, and makes its own verified TLS connection to the real
server (see egress.py).

Each bottle has a CA of its own, and that CA can only vouch for the hosts the
bottle's credentials name: its certificate carries a critical name
constraint, and the bottle's TLS clients, not bottled, are what enforce it.
So even with its key, a certificate for any other site is one the bottle
refuses, and the bottle's other traffic stays encrypted to the real server's
key, never to bottled's. A bottle whose features name no credential hosts has
no CA at all.

The constraint permits each host (RFC 5280: a DNS constraint also permits the
names below it, so `github.com` covers `api.github.com`; there is no way to
say "this name exactly"), `*.example.com` as the names below example.com, and
each IP address on its own. A name type with nothing permitted is shut
entirely: DNS names only under `invalid` (RFC 6761: none exist), and an
address family with no addresses excluded whole, because a CA constrained
only by DNS names may otherwise vouch for any IP address.

The CA is in $BOTTLE_HOME/ca/<bottle>/<digest of its hosts>, made by bottle
when the bottle starts, and installed in its trust store then (see
bottles.py); a bottle whose hosts change gets a new CA, and the old one is
removed. bottled only ever reads it: a CA it made would be one the bottle
doesn't trust. The keys never leave the machine, and the machine itself never
trusts any of them.

Certificates are made with the `openssl` command (LibreSSL on macOS), which
keeps bottle free of Python packages. Everything is configured by file rather
than by the newer command-line options, so both OpenSSL and LibreSSL accept it.
"""

import functools
import hashlib
import ipaddress
import logging
import re
import secrets
import shutil
import ssl
import subprocess
import tempfile
import threading
import time
from collections import OrderedDict
from collections.abc import Iterable
from pathlib import Path

from bottle.errors import BottleError
from bottle.store import bottle_home

log = logging.getLogger("bottle.ca")

CA_DAYS = 3650
# The most a publicly trusted certificate may have; some clients refuse longer.
LEAF_DAYS = 397
# A leaf is minted again after this long, so a long-running bottled never serves an expiring one.
LEAF_REFRESH = 30 * 86400
# How many leaves are kept: far more hosts than any bottle's credentials name,
# and a bound on what a wildcard host pattern could make bottled hold.
LEAVES_KEPT = 256

# A bottle's name is a directory here (repos.NAME_PATTERN, which a throwaway's also fits).
BOTTLE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
HOST_CHARACTERS = set("abcdefghijklmnopqrstuvwxyz0123456789.-:")
# What a DNS name constraint is set to when no DNS name is permitted: nothing is under it.
NO_DNS_NAME = "invalid"

CA_EXTENSIONS = """\
[req]
distinguished_name = dn
prompt = no
[dn]
CN = bottle egress CA for {bottle}
[ca]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
nameConstraints = critical, @names
[names]
{names}
"""

LEAF_EXTENSIONS = """\
[req]
distinguished_name = dn
prompt = no
[dn]
O = bottle egress
[leaf]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = serverAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
subjectAltName = {san}
"""


def hosts_dir(bottle: str) -> Path:
    """Where `bottle`'s CA is kept, one directory below for each set of hosts it has had."""
    if not BOTTLE_NAME.fullmatch(bottle):
        raise BottleError(f"egress CA: {bottle!r} isn't a bottle's name")
    return bottle_home() / "ca" / bottle


def ca_dir(bottle: str, hosts: Iterable[str]) -> Path:
    """The directory of `bottle`'s CA for these hosts (see certificate)."""
    patterns = _patterns(hosts)
    return hosts_dir(bottle) / hashlib.sha256("\n".join(patterns).encode()).hexdigest()[:16]


def certificate(bottle: str, hosts: Iterable[str]) -> str | None:
    """`bottle`'s CA certificate, as PEM, constrained to `hosts`; None when there are none.

    `hosts` are the host patterns the bottle's credentials name, whether or
    not they're logged in, so logging in or out doesn't change the CA. Made if
    missing; the bottle's CAs for other hosts are removed.
    """
    patterns = _patterns(hosts)
    _remove_shared_ca()
    keep = _ensure(bottle, patterns) if patterns else None
    if hosts_dir(bottle).is_dir():
        # Not another start's staging directory, which is hidden.
        for other in hosts_dir(bottle).iterdir():
            if other != keep and not other.name.startswith("."):
                shutil.rmtree(other, ignore_errors=True)
    return (keep / "ca.crt").read_text() if keep else None


def forget(bottle: str) -> None:
    """Remove `bottle`'s CAs, when the bottle is deleted."""
    shutil.rmtree(hosts_dir(bottle), ignore_errors=True)


def _remove_shared_ca() -> None:
    """Remove the one CA every bottle trusted, from before CAs were per bottle and constrained."""
    for name in ("ca.crt", "ca.key", "leaf.key"):
        path = bottle_home() / "ca" / name
        if path.is_file():
            path.unlink()


def _ensure(bottle: str, patterns: list[str]) -> Path:
    """The directory of `bottle`'s CA for `patterns`, generated if missing.

    Generated in a temporary directory beside it and renamed into place, so
    two bottle commands starting the bottle at once agree on one CA: the
    rename fails for whichever comes second, and it uses the first one's.
    """
    directory = ca_dir(bottle, patterns)
    if (directory / "ca.crt").is_file():
        return directory
    bottle_home().mkdir(mode=0o700, parents=True, exist_ok=True)
    for parent in (bottle_home() / "ca", directory.parent):
        parent.mkdir(mode=0o700, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".ca-", dir=directory.parent))
    try:
        (staging / "ca.cnf").write_text(CA_EXTENSIONS.format(bottle=bottle, names=_name_constraints(patterns)))
        _openssl("ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", str(staging / "ca.key"))
        _openssl("req", "-x509", "-new", "-key", str(staging / "ca.key"), "-sha256", "-days", str(CA_DAYS),
                 "-config", str(staging / "ca.cnf"), "-extensions", "ca", "-out", str(staging / "ca.crt"))
        # One key for every leaf: it's only as secret as the CA's, and saves a key per host.
        _openssl("ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", str(staging / "leaf.key"))
        for key in ("ca.key", "leaf.key"):
            (staging / key).chmod(0o600)
        (staging / "hosts").write_text("".join(f"{p}\n" for p in patterns))
        try:
            staging.rename(directory)
            log.info("%s: egress CA generated for %s", bottle, ", ".join(patterns))
        except OSError:
            if not (directory / "ca.crt").is_file():
                raise
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    return directory


def _patterns(hosts: Iterable[str]) -> list[str]:
    """The host patterns a CA may be constrained to, sorted and checked.

    A host name, an IP address, or `*.` and a host name for the names below
    it: nothing else can be written as a name constraint.
    """
    patterns = sorted({h.lower() for h in hosts})
    for pattern in patterns:
        if not _is_address(pattern) and not _is_dns_name(pattern.removeprefix("*.")):
            raise BottleError(
                f"egress CA: {pattern!r} can't be a credential's host: use a host name, an IP address, "
                "or *. and a host name"
            )
    return patterns


def _name_constraints(patterns: list[str]) -> str:
    """The [names] section of a CA constrained to `patterns` (see the module's docstring)."""
    addresses = [ipaddress.ip_address(p) for p in patterns if _is_address(p)]
    permitted = [("DNS", f".{p[2:]}" if p.startswith("*.") else p) for p in patterns if not _is_address(p)]
    permitted = permitted or [("DNS", NO_DNS_NAME)]
    excluded = []
    for version, everything in ((4, "0.0.0.0/0.0.0.0"), (6, "0:0:0:0:0:0:0:0/0:0:0:0:0:0:0:0")):
        family = [a for a in addresses if a.version == version]
        permitted += [("IP", f"{a.exploded}/{ipaddress.ip_network(a).netmask.exploded}") for a in family]
        if not family:
            excluded.append(("IP", everything))
    lines = [f"permitted;{kind}.{i} = {value}" for i, (kind, value) in enumerate(permitted)]
    lines += [f"excluded;{kind}.{i} = {value}" for i, (kind, value) in enumerate(excluded)]
    return "\n".join(lines)


def _permits(patterns: list[str], host: str) -> bool:
    """Whether `host` is one of `patterns`: the hosts a bottle's proxy terminates tunnels for."""
    return any(host == p or (p.startswith("*.") and host.endswith(p[1:]) and len(host) > len(p) - 1)
               for p in patterns)


# By CA certificate and host: a CA is made again under the same directory
# when its bottle is deleted and made again. The least recently used go
# first, past LEAVES_KEPT.
_leaves: OrderedDict[tuple[Path, str, str], tuple[float, ssl.SSLContext]] = OrderedDict()
_leaves_lock = threading.Lock()


def server_context(bottle: str, hosts: Iterable[str], host: str) -> ssl.SSLContext:
    """A TLS server context presenting a certificate for `host`, signed by `bottle`'s CA for `hosts`.

    `host` must be one of `hosts`, and the CA must exist: bottle makes it
    when the bottle starts. Minted on first use and kept for LEAF_REFRESH.
    HTTP/1.1 only (ALPN), since the proxy reads the request itself.
    """
    patterns, host = _patterns(hosts), host.lower()
    if not _permits(patterns, host):
        raise BottleError(f"egress CA: {bottle}'s CA isn't for {host}")
    directory = ca_dir(bottle, patterns)
    try:
        authority = (directory / "ca.crt").read_text()
    except FileNotFoundError:
        raise BottleError(f"egress CA: {bottle} has none for its hosts yet; it's made when the bottle starts") from None
    key = (directory, authority, host)
    with _leaves_lock:
        cached = _leaves.get(key)
        if cached and time.time() - cached[0] < LEAF_REFRESH:
            _leaves.move_to_end(key)
            return cached[1]
        context = _mint(directory, host)
        _leaves[key] = (time.time(), context)
        _leaves.move_to_end(key)
        while len(_leaves) > LEAVES_KEPT:
            _leaves.popitem(last=False)
        return context


def _mint(directory: Path, host: str) -> ssl.SSLContext:
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        (work / "leaf.cnf").write_text(LEAF_EXTENSIONS.format(san=_subject_alt_name(host)))
        _openssl("req", "-new", "-key", str(directory / "leaf.key"), "-config", str(work / "leaf.cnf"),
                 "-out", str(work / "leaf.csr"))
        _openssl("x509", "-req", "-in", str(work / "leaf.csr"), "-CA", str(directory / "ca.crt"),
                 "-CAkey", str(directory / "ca.key"), "-set_serial", f"0x{secrets.token_hex(16)}",
                 "-days", str(LEAF_DAYS), "-sha256", "-extfile", str(work / "leaf.cnf"), "-extensions", "leaf",
                 "-out", str(work / "leaf.crt"))
        # The chain a client is sent: the leaf, then the CA it trusts.
        (work / "chain.pem").write_text((work / "leaf.crt").read_text() + (directory / "ca.crt").read_text())
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.minimum_version = ssl.TLSVersion.TLSv1_2
        context.set_alpn_protocols(["http/1.1"])
        context.load_cert_chain(work / "chain.pem", directory / "leaf.key")
    log.info("egress CA: minted a certificate for %s", host)
    return context


def _is_address(host: str) -> bool:
    # Characters first: an IPv6 scope ID (fe80::1%...) parses, and may hold anything.
    if not host or not set(host) <= HOST_CHARACTERS:
        return False
    try:
        ipaddress.ip_address(host)
        return True
    except ValueError:
        return False


def _is_dns_name(host: str) -> bool:
    return bool(host) and set(host) <= HOST_CHARACTERS - {":"} and "" not in host.split(".")


def _subject_alt_name(host: str) -> str:
    """The certificate's subjectAltName for `host`, refusing anything else.

    `host` is written into an openssl config file as it is, so it is checked
    character by character first: an IPv6 scope ID (fe80::1%...) parses as an
    address but may hold anything, a newline included.
    """
    if _is_address(host):
        return f"IP:{ipaddress.ip_address(host)}"
    if not _is_dns_name(host):
        raise BottleError(f"egress CA: {host!r} isn't a host name a certificate can be made for")
    return f"DNS:{host}"


@functools.cache
def _openssl_path() -> str:
    path = shutil.which("openssl")
    if path is None:
        raise BottleError("the egress CA needs the openssl command, which macOS includes; it isn't on PATH")
    return path


def _openssl(*args: str) -> None:
    result = subprocess.run([_openssl_path(), *args], capture_output=True, text=True)
    if result.returncode != 0:
        raise BottleError(f"openssl {args[0]} failed: {result.stderr.strip() or result.returncode}")
