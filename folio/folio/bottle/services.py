"""Services: what a machine's policy routes to folio serve.

A machine's policy (its network.json, given when bottle makes it: see network()) routes some of its connections
to a unix socket of folio serve's: bottle.host's services, ssh to the bottles it may reach, and each credential's
host. bottle-cli decides everything else itself, and gives folio serve each routed connection with a PROXY
protocol header saying what it was for and which machine it's from (folio-cli's serve/proxy.rs and gateway.rs).
"""

from folio.bottle import cache, host


def network(credentials: list[tuple[str, int]], caches: list[cache.Cached], allow_private: tuple[str, ...] = (),
            sockets: dict[str, str] | None = None) -> dict:
    """A machine's network policy, as bottle-cli takes it (agent-client's policy): what it caches, the private
    names it may reach, and what it routes to folio serve's `sockets`, by service: bottle.host's (git, messages),
    ssh to bottles, and each credential's host pattern and port (auth.credential_destinations), logged in or not.
    Without sockets, its routes say their service instead, for with_sockets to fill in where it's made."""
    routes = [
        {"address": host.ADDRESS, "port": 80, "service": "host"},
        {"network": host.SSH_NETWORK, "port": host.SSH_PORT, "service": "ssh"},
        *({"host": h, "port": p, "service": "credentials"} for h, p in sorted(set(credentials))),
    ]
    policy = {"allow_private": list(allow_private), "cache": cache_rules(caches), "routes": routes}
    return with_sockets(policy, sockets) if sockets else policy


def cache_rules(caches: list[cache.Cached]) -> list[dict]:
    """What a policy says of the hosts whose downloads bottle-cli keeps, as cache.Cached has them."""
    return [{"host": c.host, "include": list(c.include), "exclude": list(c.exclude), "ports": list(c.ports),
             "immutable": c.immutable} for c in caches]


def with_sockets(policy: dict, sockets: dict[str, str]) -> dict:
    """`policy`, each route to its service's socket among `sockets`."""
    routes = [{**{k: v for k, v in route.items() if k != "service"}, "socket": str(sockets[route["service"]])}
              for route in policy.get("routes", ())]
    return {**policy, "routes": routes}
