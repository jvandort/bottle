import unittest

from folio.bottle import cache, host, services


class NetworkTest(unittest.TestCase):
    def test_a_machines_policy_routes_bottle_host_ssh_and_credentials_here_and_says_what_it_caches(self) -> None:
        policy = services.network([("api.example", 443), ("ci.example", 8111), ("api.example", 443)],
                                  [cache.Cached("deb.example", include=("/debian/*.deb",), ports=(80,), immutable=True)],
                                  sockets={"host": "/h.sock", "ssh": "/s.sock", "credentials": "/c.sock"})
        self.assertEqual(policy["routes"], [
            {"address": host.ADDRESS, "port": 80, "socket": "/h.sock"},
            {"network": host.SSH_NETWORK, "port": host.SSH_PORT, "socket": "/s.sock"},
            {"host": "api.example", "port": 443, "socket": "/c.sock"},
            {"host": "ci.example", "port": 8111, "socket": "/c.sock"},
        ])
        self.assertEqual(policy["cache"], [{"host": "deb.example", "include": ["/debian/*.deb"], "exclude": [],
                                            "ports": [80], "immutable": True}])
        self.assertEqual(policy["allow_private"], [])
        # Made without sockets, each route says its service, for the sockets where it's made.
        self.assertEqual(services.network([], [])["routes"][0]["service"], "host")
        given = services.with_sockets(services.network([], []), {"host": "/h", "ssh": "/s", "credentials": "/c"})
        self.assertEqual(given["routes"][1], {"network": host.SSH_NETWORK, "port": host.SSH_PORT, "socket": "/s"})


if __name__ == "__main__":
    unittest.main()
