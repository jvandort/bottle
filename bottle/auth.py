"""Credentials: logged in to once on the host, delivered to the bottles that need them.

Features declare the credentials they need (customizations.bottle.credentials
in their devcontainer-feature.json): a name, how to log in, and the
environment variable it's delivered as. bottle stores each credential in the
macOS Keychain and delivers it to every bottle whose features declare it,
whenever the bottle starts.

Logging in runs the feature's own login command (e.g. `claude setup-token`) in
a throwaway bottle with that feature, attached to your terminal, and captures
the credential from its output. The agent in a bottle can read a credential
delivered to it.
"""

import getpass
import hashlib
import os
import pty
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from bottle import features
from bottle.errors import BottleError
from bottle.store import bottle_home

# Terminal escape codes: colors, cursor movement, window titles.
ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][0-9A-Za-z]|\x1b[=>78DEHM]")


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


def delete(name: str) -> bool:
    """Remove the credential. False if it wasn't stored."""
    result = subprocess.run(
        ["security", "delete-generic-password", "-s", _service(), "-a", name], capture_output=True, text=True
    )
    return result.returncode == 0


# --- commands -------------------------------------------------------------------


def status() -> list[tuple[Declared, bool]]:
    return [(d, get(name) is not None) for name, d in sorted(declared().items())]


def login(name: str) -> None:
    """Log in with the credential's declared flow, or ask for the value if it has none."""
    d = get_declared(name)
    if not sys.stdin.isatty():
        raise BottleError(f"logging in needs a terminal; to set {name} from a script, use `bottle auth set {name}`")
    login = d.credential.login
    if login is None:
        value = getpass.getpass(f"{d.credential.description}: ")
    else:
        from bottle import bottles  # bottles imports features, runtime, daemon; only logging in needs it

        print(f"Starting a throwaway bottle with {d.feature} to run `{' '.join(login.command)}`...", file=sys.stderr)
        with bottles.throwaway([d.feature]) as argv_for:
            output = run_captured(argv_for(list(login.command)))
        value = capture(login.capture, output)
        if value is None:
            value = getpass.getpass(f"bottle couldn't find the {name} credential in the output; paste it: ")
    put(name, value.strip())


def set_value(name: str, value: str) -> None:
    get_declared(name)
    put(name, value.strip())


def logout(name: str) -> bool:
    get_declared(name)
    return delete(name)


def capture(pattern: str, output: str) -> str | None:
    """The credential in a login command's output: `pattern`'s group, with escape codes removed first.

    The pattern is matched across lines, and whitespace is removed from what it
    captures: a narrow terminal can wrap a long token over several lines, and
    credentials never contain whitespace.
    """
    match = re.search(pattern, ANSI.sub("", output), re.DOTALL)
    value = re.sub(r"\s+", "", match.group(1)) if match else ""
    return value or None


def run_captured(argv: list[str]) -> str:
    """Run `argv` attached to this terminal, returning everything it printed."""
    chunks: list[bytes] = []

    def read(fd: int) -> bytes:
        data = os.read(fd, 4096)
        chunks.append(data)
        return data

    status_ = pty.spawn(argv, read)
    output = b"".join(chunks).decode(errors="replace")
    if os.waitstatus_to_exitcode(status_) != 0 and "\x1b" not in output and not output.strip():
        raise BottleError(f"`{' '.join(argv)}` failed")
    return output


# --- delivery -------------------------------------------------------------------


def credentials_for(specs: tuple[str, ...] | list[str]) -> list[features.Credential]:
    """The credentials a bottle with these features gets."""
    return [c for f in features.resolve(list(specs)) for c in f.credentials]


def ensure_logged_in(specs: tuple[str, ...] | list[str]) -> None:
    """Log in to any credential these features need that isn't set yet.

    Interactive only: without a terminal, say how to log in and carry on.
    """
    for credential in credentials_for(specs):
        if get(credential.name) is not None:
            continue
        if not sys.stdin.isatty():
            print(f"bottle: {credential.name} isn't logged in; run `bottle auth login {credential.name}`", file=sys.stderr)
            continue
        print(f"{credential.description}: not logged in yet. Logging in now...", file=sys.stderr)
        login(credential.name)


def env_for(specs: tuple[str, ...] | list[str]) -> dict[str, str | None]:
    """Each credential's variable and its stored value (None if not logged in)."""
    return {c.env: get(c.name) for c in credentials_for(specs)}
