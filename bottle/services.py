"""Services: where bottle-cli gives bottled the connections a machine's policy routes to it.

A machine's policy (its network.json, given when bottle makes it: see
network()) routes some of its connections to a unix socket here:
bottle.host's services, ssh to the bottles it may reach, and each credential's
host. bottle-cli decides everything else itself. Each routed connection starts
with a PROXY protocol header, version 2 (haproxy's proxy-protocol.txt): the
address and port the bottle connected to, then, as fields, the name it asked
for (SNI or Host), whether it spoke TLS (which bottle-cli has taken off, so
what follows is the plain conversation), and, in a field of bottle's own, the
machine it's from. bottled answers it as the gateway of the bottle that machine
is (see egress.py), bound when the bottle was made or bottled started; a
connection from a machine no bottle has is dropped.
"""

import asyncio
import ipaddress
import logging
import time
from dataclasses import dataclass
from pathlib import Path

from bottle import cache, host
from bottle.egress import Direct, Flow, Gateway, Refused

log = logging.getLogger("bottle.services")
log.addHandler(logging.NullHandler())

SIGNATURE = b"\r\n\r\n\x00\r\nQUIT\n"
VERSION_2_PROXY = 0x21
TCP_OVER_IPV4, TCP_OVER_IPV6 = 0x11, 0x21
PP2_TYPE_AUTHORITY = 0x02
PP2_TYPE_SSL = 0x20
PP2_CLIENT_SSL = 0x01
# bottle-cli's own field: the machine the connection is from (agent-client's net.rs, PP2_TYPE_MACHINE).
PP2_TYPE_MACHINE = 0xE0
HEADER_TIMEOUT = 10


@dataclass(frozen=True)
class Header:
    """What bottle-cli says of a routed connection."""

    address: str
    port: int
    name: str | None
    tls: bool
    machine: str | None


async def read_header(reader: asyncio.StreamReader) -> Header:
    """A PROXY protocol v2 header, as bottle-cli writes one; ValueError if it isn't one."""
    fixed = await reader.readexactly(16)
    if fixed[:12] != SIGNATURE or fixed[12] != VERSION_2_PROXY or fixed[13] not in (TCP_OVER_IPV4, TCP_OVER_IPV6):
        raise ValueError("not a PROXY protocol v2 header for TCP")
    body = await reader.readexactly(int.from_bytes(fixed[14:16], "big"))
    size = 4 if fixed[13] == TCP_OVER_IPV4 else 16
    if len(body) < 2 * size + 4:
        raise ValueError("a PROXY protocol header too short for its addresses")
    address = str(ipaddress.ip_address(body[size:2 * size]))
    port = int.from_bytes(body[2 * size + 2:2 * size + 4], "big")
    fields: dict[int, bytes] = {}
    at = 2 * size + 4
    while at + 3 <= len(body):
        length = int.from_bytes(body[at + 1:at + 3], "big")
        fields[body[at]] = body[at + 3:at + 3 + length]
        at += 3 + length
    name = fields.get(PP2_TYPE_AUTHORITY)
    machine = fields.get(PP2_TYPE_MACHINE)
    return Header(
        address, port, name.decode() if name else None,
        bool(fields.get(PP2_TYPE_SSL, b"\0")[0] & PP2_CLIENT_SSL),
        machine.decode() if machine else None,
    )


def network(credentials: list[tuple[str, int]], caches: list[cache.Cached], allow_private: tuple[str, ...] = (),
            socket: str | None = None) -> dict:
    """A machine's network policy, as bottle-cli takes it (agent-client's policy): what it caches, the private
    names it may reach, and what it routes to bottled's `socket`: bottle.host's services, ssh to bottles, and
    each credential's host pattern and port (auth.credential_destinations), logged in or not. Without a
    socket, its routes have none yet: the runner's, filled in where the machine is made (with_socket)."""
    routes = [
        {"address": host.ADDRESS, "port": 80},
        {"network": host.SSH_NETWORK, "port": host.SSH_PORT},
        *({"host": h, "port": p} for h, p in sorted(set(credentials))),
    ]
    policy = {"allow_private": list(allow_private), "cache": cache_rules(caches), "routes": routes}
    return with_socket(policy, socket) if socket else policy


def cache_rules(caches: list[cache.Cached]) -> list[dict]:
    """What a policy says of the hosts whose downloads bottle-cli keeps, as cache.Cached has them."""
    return [{"host": c.host, "include": list(c.include), "exclude": list(c.exclude), "ports": list(c.ports),
             "immutable": c.immutable} for c in caches]


def with_socket(policy: dict, socket: str) -> dict:
    """`policy`, its routes to `socket`."""
    return {**policy, "routes": [{**route, "socket": socket} for route in policy.get("routes", ())]}


@dataclass
class Binding:
    bottle: str
    gateway: Gateway


class Services:
    """The bottles bound here, by their machines' names, and the socket their routed connections come to."""

    def __init__(self) -> None:
        self.bindings: dict[str, Binding] = {}

    def bind(self, bottle: str, machine: str, gateway: Gateway) -> None:
        """Answer the connections `machine` routes here as `bottle`'s, with `gateway`."""
        self.release(bottle)
        self.bindings[machine] = Binding(bottle, gateway)

    def release(self, bottle: str) -> None:
        for machine in [m for m, b in self.bindings.items() if b.bottle == bottle]:
            del self.bindings[machine]

    def bound(self, bottle: str) -> bool:
        return any(b.bottle == bottle for b in self.bindings.values())

    async def start(self, path: Path) -> asyncio.Server:
        path.unlink(missing_ok=True)
        server = await asyncio.start_unix_server(self.handle, str(path))
        path.chmod(0o600)
        return server

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        started = time.monotonic()
        try:
            header = await asyncio.wait_for(read_header(reader), HEADER_TIMEOUT)
        except (ValueError, asyncio.IncompleteReadError, TimeoutError, ConnectionError) as e:
            log.warning("services: refused a connection: %s", e or type(e).__name__)
            writer.close()
            return
        binding = self.bindings.get(header.machine or "")
        target = f"{header.address}:{header.port}" + (f" ({header.name})" if header.name else "")
        if binding is None:
            log.warning("services: refused %s from %s, which no bottle here is", target, header.machine)
            writer.close()
            return
        gateway = binding.gateway
        flow = Flow(header.address, header.port, name=header.name, unwrapped=header.tls)
        try:
            decision = await gateway.decide(flow)
        except Refused as e:
            gateway.log_flow(target, None, e.outcome, None, [0], [0], started)
            writer.close()
            return
        except Exception:
            log.exception("%s: deciding %s", binding.bottle, target)
            writer.close()
            return
        if isinstance(decision, Direct):
            # Routed here, but none of bottled's: the machine's policy is out of date.
            gateway.log_flow(target, None, "not-a-service", None, [0], [0], started)
            writer.close()
            return
        await gateway.serve(flow, decision, reader, writer)
