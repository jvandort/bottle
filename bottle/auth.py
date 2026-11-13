"""Credentials: logged in to once on the controller, and used by the bottles that need them.

Features declare the credentials they need (customizations.bottle.credentials
in their devcontainer-feature.json): a name, and where the credential goes.
Each is stored in the macOS Keychain, set from stdin (`folio auth set`).

A credential never enters a bottle. folio serve hands it to that bottle's
gateway (injections_for), which puts it in place of the stand-in on the bottle's
requests to the hosts the feature named, on HTTPS connections the machine
makes (see egress.py). The agent can spend the credential against those
hosts, and can't read it -- so a bottle that leaks everything it holds leaks
no credential.

What a bottle gets instead is a stand-in (standins_for): a fake token in the
variable a feature named, because a CLI doesn't know its token is being
replaced for it and won't work until it thinks it's logged in. A request that
presents the stand-in is one that asks for the credential; the rest reach the
host as they would from the machine.

A credential can also come from a file a tool on the machine already keeps
its credentials in (the credential's properties, see features.py), such as
Gradle's ~/.gradle/develocity/keys.properties: folio serve reads the line for each
host, and the Keychain is for the hosts the file doesn't name.

A bottle picks up a credential set or removed, or a change to such a file, when it next starts.
"""

import hashlib
import logging
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from bottle import egress, features
from bottle.errors import BottleError
from bottle.store import bottle_home

log = logging.getLogger("bottle.auth")

HTTPS_PORT = 443


@dataclass(frozen=True)
class Declared:
    credential: features.Credential
    feature: str  # a feature that declares it


def declared() -> dict[str, Declared]:
    """Every credential the available features declare, by name."""
    result: dict[str, Declared] = {}
    for feature_id in features.available():
        for credential in features.load(feature_id).credentials:
            other = result.get(credential.name)
            if other and other.credential != credential:
                raise BottleError(
                    f"features {other.feature} and {feature_id} declare credential {credential.name!r} differently"
                )
            result.setdefault(credential.name, Declared(credential, feature_id))
    return result


def get_declared(name: str) -> Declared:
    known = declared()
    if name not in known:
        raise BottleError(f"no feature declares a credential named {name!r} (known: {', '.join(sorted(known)) or 'none'})")
    return known[name]


# --- storage: the macOS Keychain -------------------------------------------------


def _service() -> str:
    """The Keychain service for this BOTTLE_HOME, so separate homes keep separate credentials."""
    home = bottle_home().expanduser().resolve()
    if home == (Path.home() / ".bottle").resolve():
        return "bottle"
    return f"bottle:{hashlib.sha256(str(home).encode()).hexdigest()[:12]}"


def get(name: str) -> str | None:
    result = subprocess.run(
        ["security", "find-generic-password", "-s", _service(), "-a", name, "-w"], capture_output=True, text=True
    )
    return result.stdout.rstrip("\n") if result.returncode == 0 else None


def put(name: str, value: str) -> None:
    """Store `value`. It goes to `security` on stdin, never on a command line."""
    if not value or any(c in value for c in '"\\\n\r') or not value.isprintable():
        raise BottleError(f"{name}: the value is empty or has characters bottle can't store (quotes, backslashes, control)")
    command = f'add-generic-password -U -s "{_service()}" -a "{name}" -w "{value}"\n'
    result = subprocess.run(["security", "-i"], input=command, capture_output=True, text=True)
    if result.returncode != 0 or "error" in result.stderr.lower() or get(name) != value:
        raise BottleError(f"couldn't store {name} in the Keychain: {result.stderr.strip() or 'unknown error'}")
    log.info("credential %s stored", name)


def delete(name: str) -> bool:
    """Remove the credential. False if it wasn't stored."""
    result = subprocess.run(
        ["security", "delete-generic-password", "-s", _service(), "-a", name], capture_output=True, text=True
    )
    if result.returncode == 0:
        log.info("credential %s removed", name)
    return result.returncode == 0


# --- commands -------------------------------------------------------------------


def status() -> list[tuple[Declared, bool]]:
    """Every declared credential, and whether the Keychain has it."""
    return [(d, get(name) is not None) for name, d in sorted(declared().items())]


# --- properties files --------------------------------------------------------------


def read_properties(path: Path) -> dict[str, str]:
    """A Java properties file's `key=value` (or `key:value`) lines, by key; {} when there's no file.

    Enough of the format for a file of credentials: comments (# and !) and
    blank lines are skipped, and a backslash escapes the character after it.
    A key ends at the first `=`, and only at a `:` on a line with no `=`, so a
    URL is a key as written (Java would end it at `https`). Continuation lines
    and \\u escapes aren't supported, and no credential needs them.
    """
    try:
        text = path.expanduser().read_text()
    except FileNotFoundError:
        return {}
    except OSError as e:
        raise BottleError(f"can't read {path}: {e.strerror or e}") from None
    result = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line[0] in "#!":
            continue
        separators = "=" if "=" in line else ":"
        key, value, escaped = "", None, False
        for i, c in enumerate(line):
            if escaped:
                key, escaped = key + c, False
            elif c == "\\":
                escaped = True
            elif c in separators:
                value = line[i + 1:].strip()
                break
            else:
                key += c
        if value is not None:
            result[key.strip()] = value
    return result


def from_properties(credential: features.Credential, host: str) -> str | None:
    """The credential for `host`, one of its inject.hosts, from its properties file; None when the file has no line for it.

    A line's key may be the host or a URL of it; a host with a port matches its
    host:port line, else its bare host's.
    """
    if not credential.properties:
        return None
    keys = {}
    for key, value in read_properties(Path(credential.properties)).items():
        if value:
            keys[key.lower().removeprefix("https://").rstrip("/")] = value
    pattern, port = destination(host)
    return keys.get(host.lower()) or keys.get(pattern.lower())


def value_for(credential: features.Credential, host: str) -> str | None:
    """What's attached to `host` for `credential`: its properties file's line for the host, else the Keychain's."""
    return from_properties(credential, host) or get(credential.name)


def set_value(name: str, value: str) -> None:
    get_declared(name)
    put(name, value.strip())


def remove(name: str) -> bool:
    get_declared(name)
    return delete(name)


# --- injection ------------------------------------------------------------------


def credentials_for(specs: tuple[str, ...] | list[str]) -> list[features.Credential]:
    """The credentials a bottle with these features gets, with their features' options filled in."""
    return [c for f in features.resolve(list(specs)) for c in f.resolved_credentials()]


def warn_unset(specs: tuple[str, ...] | list[str]) -> None:
    """Say which credentials these features need that aren't set, and how to set them; the bottle goes on
    without, reaching their hosts unauthenticated."""
    for credential in credentials_for(specs):
        if all(from_properties(credential, host) for host in credential.inject.hosts) or get(credential.name) is not None:
            continue
        print(f"bottle: {credential.name} ({credential.description}) isn't set: `folio auth set {credential.name}`",
              file=sys.stderr)


def injections_for(specs: tuple[str, ...] | list[str]) -> list[egress.Injection]:
    """What folio serve attaches for a bottle with these features: one per injected credential.

    Read from its properties file or the Keychain here, in folio serve, so the
    value goes no further than the gateway. A credential that isn't logged in is left out; the bottle then
    reaches the host unauthenticated, and the server says so.

    A request to a host that several patterns match is served by the first
    credential that claims it, in the features' install order. Two credentials
    claiming the same host is a mistake in the features, and is refused here
    rather than resolved arbitrarily.
    """
    injections, claimed = [], {}
    for credential in credentials_for(specs):
        for host in credential.inject.hosts:
            owner = claimed.setdefault(host.lower(), credential.name)
            if owner != credential.name:
                raise BottleError(f"credentials {owner!r} and {credential.name!r} both claim {host}")
        for host in credential.inject.hosts:
            value = value_for(credential, host)
            if value is None:
                continue
            pattern, port = destination(host)
            injections.append(egress.Injection(
                host=pattern,
                port=port,
                header=credential.inject.header,
                value=credential.inject.value.replace(features.CREDENTIAL_PLACEHOLDER, value),
                standin=features.STANDIN,
                credential=value,
                rewrites=credential.inject.rewrites,
                forbid=credential.inject.forbid,
                allow=credential.inject.allow,
                feature=credential.name,
            ))
    return injections


def credential_destinations(specs: tuple[str, ...] | list[str]) -> list[tuple[str, int]]:
    """The host patterns and ports a bottle with these features has credentials attached at, logged in or not:
    what its machine routes to folio serve (see services.network)."""
    return sorted({(pattern.lower(), port) for c in credentials_for(specs) for pattern, port in
                   (destination(h) for h in c.inject.hosts)})


def standins_for(specs: tuple[str, ...] | list[str]) -> dict[str, str]:
    """The fake tokens a bottle with these features gets, by variable name.

    A stand-in is not a credential and not a secret: it is what makes a tool
    believe it's logged in, so it will make the request folio serve then
    authenticates. bottle sets these when it creates a bottle.
    """
    return {c.inject.standin: features.STANDIN for c in credentials_for(specs) if c.inject.standin}


def destination(host: str) -> tuple[str, int]:
    """One of inject.hosts as a host pattern and a port: `example.com` or `example.com:8111`.

    Always https, so the port is 443 unless the host names another one.
    """
    pattern, _, port = host.rpartition(":")
    if not pattern:
        return host, HTTPS_PORT
    if not port.isdigit() or not 0 < int(port) < 65536:
        raise BottleError(f"{host!r} isn't a host for a credential: a host or a host:port")
    return pattern, int(port)
