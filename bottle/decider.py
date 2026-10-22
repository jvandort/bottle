"""The decider: where a driver that carries the guest's sockets asks about each connection before making it.

smolvm's mediated egress (version 2 of its decider protocol) connects here
once per guest TCP flow, from host loopback, and writes a prelude: a token
the host gave it for the machine, the machine's identity, the destination,
and the flow's first bytes. bottled answers one byte: allow direct (smolvm
connects it), deny, or redirect, after which the same connection carries the
flow and bottled is its far end (see Gateway.decide and .serve in
egress.py).

  offset  bytes  field
       0      8  magic, SMOLMEG2
       8     32  token, as given in SMOLVM_INTERCEPTOR_TOKEN
      40     16  machine ID, minted by smolvm for the launch
      56     16  parent ID, zero unless the machine is a branch
      72      1  IP family, 4 or 6
      73      2  destination port
      75   4/16  destination address
               2  length of the first bytes, at most 8192
               n  the first bytes

A token is the bottle: each bottle has its own, minted when it's bound
(when its machine starts), and the first machine ID that comes with it is
the only one accepted after. smolvm keeps the token it was started with for
the machine's whole run, so tokens are kept on disk, owner-only, and a
restarted bottled honours the ones it gave out before.
"""

import asyncio
import hashlib
import ipaddress
import json
import logging
import os
import secrets
import time
from dataclasses import dataclass
from pathlib import Path

from bottle.egress import Direct, Gateway, Flow, Refused, name_in
from bottle.store import bottle_home

log = logging.getLogger("bottle.decider")
log.addHandler(logging.NullHandler())

MAGIC = b"SMOLMEG2"
TOKEN_BYTES = 32
ID_BYTES = 16
MAX_FIRST_BYTES = 8192
PRELUDE_TIMEOUT = 10
ALLOW_DIRECT, DENY, REDIRECT = b"\x00", b"\x01", b"\x02"
# On host loopback, at a port that stays put: smolvm keeps the address it was
# given for a launch's life, through any restart of bottled.
DEFAULT_PORT = 47329


def port() -> int:
    return int(os.environ.get("BOTTLE_DECIDER_PORT") or DEFAULT_PORT)


class BadPrelude(Exception):
    pass


@dataclass(frozen=True)
class Prelude:
    token: bytes
    machine_id: bytes
    address: str
    port: int
    first: bytes


async def read_prelude(reader: asyncio.StreamReader) -> Prelude:
    """One flow's prelude. Lengths are checked before anything is read on their account."""
    fixed = await reader.readexactly(len(MAGIC) + TOKEN_BYTES + 2 * ID_BYTES + 1 + 2)
    if fixed[:8] != MAGIC:
        raise BadPrelude("not a version 2 prelude")
    machine_id = fixed[40:56]
    if machine_id == bytes(ID_BYTES):
        raise BadPrelude("no machine identity")
    family = fixed[72]
    if family not in (4, 6):
        raise BadPrelude(f"IP family {family}")
    address = ipaddress.ip_address(await reader.readexactly(4 if family == 4 else 16))
    length = int.from_bytes(await reader.readexactly(2), "big")
    if length > MAX_FIRST_BYTES:
        raise BadPrelude(f"{length} first bytes")
    first = await reader.readexactly(length)
    return Prelude(fixed[8:40], machine_id, str(address), int.from_bytes(fixed[73:75], "big"), first)


def tokens_path() -> Path:
    return bottle_home() / "decider" / "tokens.json"


@dataclass
class Binding:
    bottle: str
    gateway: Gateway
    machine_id: bytes | None = None  # the first one seen with the bottle's token


class Decider:
    def __init__(self, path: Path | None = None) -> None:
        self.path = path or tokens_path()
        # By the token's digest: looking a token up never compares the secret itself.
        self.bindings: dict[bytes, Binding] = {}
        self.tokens: dict[str, bytes] = self._load()

    def bind(self, bottle: str, gateway: Gateway, token: bytes | None = None) -> bytes:
        """Answer `bottle`'s flows with `gateway`; returns its token, a new one unless `token` is given."""
        self._unbind(bottle)
        token = token or secrets.token_bytes(TOKEN_BYTES)
        self.bindings[_digest(token)] = Binding(bottle, gateway)
        if self.tokens.get(bottle) != token:
            self.tokens[bottle] = token
            self._save()
        return token

    def release(self, bottle: str) -> None:
        """Stop answering `bottle`'s flows, and forget its token: its machine has stopped."""
        self._unbind(bottle)
        if self.tokens.pop(bottle, None) is not None:
            self._save()

    def _unbind(self, bottle: str) -> None:
        if bottle in self.tokens:
            self.bindings.pop(_digest(self.tokens[bottle]), None)

    async def start(self, port: int) -> asyncio.Server:
        return await asyncio.start_server(self.handle, "127.0.0.1", port)

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        started = time.monotonic()
        try:
            prelude = await asyncio.wait_for(read_prelude(reader), PRELUDE_TIMEOUT)
        except (BadPrelude, ValueError, asyncio.IncompleteReadError, TimeoutError, ConnectionError) as e:
            log.warning("decider: refused a connection: %s", e or type(e).__name__)
            writer.close()
            return
        binding = self.bindings.get(_digest(prelude.token))
        if binding is None:
            log.warning("decider: refused a flow with a token no bottle has")
            writer.close()
            return
        gateway = binding.gateway
        flow = Flow(prelude.address, prelude.port, prelude.first)
        name = name_in(flow.first)
        target = f"{flow.address}:{flow.port}" + (f" ({name})" if name else "")
        if binding.machine_id is None:
            binding.machine_id = prelude.machine_id
        elif binding.machine_id != prelude.machine_id:
            writer.write(DENY)
            gateway.log_flow(target, None, "another-machine", None, [0], [0], started)
            writer.close()
            return
        try:
            decision = await gateway.decide(flow)
        except Refused as e:
            writer.write(DENY)
            gateway.log_flow(target, None, e.outcome, None, [0], [0], started)
            writer.close()
            return
        except Exception:
            log.exception("%s: deciding %s", binding.bottle, target)
            writer.write(DENY)
            writer.close()
            return
        if isinstance(decision, Direct):
            writer.write(ALLOW_DIRECT)
            gateway.log_flow(target, flow.address, "direct", None, [0], [0], started)
            writer.close()
            return
        writer.write(REDIRECT)
        await writer.drain()
        await gateway.serve(flow, decision, reader, writer)

    def _load(self) -> dict[str, bytes]:
        try:
            saved = json.loads(self.path.read_text())
        except FileNotFoundError:
            return {}
        return {bottle: bytes.fromhex(token) for bottle, token in saved.items()}

    def _save(self) -> None:
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        staging = self.path.with_name(f".{self.path.name}.tmp")
        # Owner-only from the start: a token lets whoever holds it speak as its bottle.
        fd = os.open(staging, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump({bottle: token.hex() for bottle, token in self.tokens.items()}, f, indent=2, sort_keys=True)
        staging.replace(self.path)


def _digest(token: bytes) -> bytes:
    return hashlib.sha256(token).digest()
