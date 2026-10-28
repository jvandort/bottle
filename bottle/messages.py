"""Addresses for messages between bottles: who may write to whom, without a table.

A bottle sends a message to `to` (see host.py, POST /messages), which is
either a bottle's name or an address, `<name>~<signature>`. Names can't hold a
`~` (bottles.NAME_PATTERN), so the two never meet.

An address is issued to one bottle, the holder, for reaching another, the
target: the signature is an HMAC, under a key that never leaves the host, of
both their names. bottled hands one out as the `from` of every message it
delivers -- the sender's address, issued to the recipient -- so a bottle can
answer whoever wrote to it, and no one else. The address is good only from its
holder: one passed to another bottle is refused there, and can't be guessed.

A bare name is for bottles with the message-any permission (see specs.py),
which start conversations; every other bottle can only answer.

Nothing is kept per conversation, so nothing goes stale: a thread whose bottle
is deleted and made again under the same name (a folio thread, paused and
resumed) answers with the addresses it had. Deleting the key revokes every
address at once.
"""

import base64
import hashlib
import hmac
import os
import secrets
from pathlib import Path

from bottle.store import bottle_home

MARK = "~"
KEY_BYTES = 32
# 26 base32 characters: 130 bits of the HMAC, well past guessing.
SIGNATURE_CHARS = 26


def key_path() -> Path:
    return bottle_home() / "messages.key"


def _key() -> bytes:
    """The key, made the first time it's needed; only its owner can read it."""
    path = key_path()
    try:
        return path.read_bytes()
    except FileNotFoundError:
        pass
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:  # made by someone else since
        return path.read_bytes()
    with os.fdopen(fd, "wb") as f:
        f.write(secrets.token_bytes(KEY_BYTES))
    return path.read_bytes()


def _signature(target: str, holder: str) -> str:
    digest = hmac.new(_key(), f"{target}\0{holder}".encode(), hashlib.sha256).digest()
    return base64.b32encode(digest).decode().lower()[:SIGNATURE_CHARS]


def address(target: str, holder: str) -> str:
    """The address by which `holder` reaches `target`."""
    return f"{target}{MARK}{_signature(target, holder)}"


def is_address(to: str) -> bool:
    return MARK in to


def target(to: str, holder: str) -> str | None:
    """The bottle address `to` reaches, when `holder` sends to it; None if it isn't one issued to `holder`."""
    name, _, signature = to.rpartition(MARK)
    if not name or not hmac.compare_digest(signature, _signature(name, holder)):
        return None
    return name
