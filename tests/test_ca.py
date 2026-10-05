import asyncio
import os
import ssl
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

from bottle import ca
from bottle.errors import BottleError

HOSTS = ("api.github.com", "github.com", "10.1.2.3")


class CATestCase(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.home = Path(tmp.name) / "home"
        patcher = mock.patch.dict(os.environ, {"BOTTLE_HOME": str(self.home)})
        patcher.start()
        self.addCleanup(patcher.stop)

    def handshake(self, server: ssl.SSLContext, host: str, trust: ssl.SSLContext) -> dict:
        """Connect to a server presenting `server`, as a client trusting `trust` would, asking for `host`."""

        async def run() -> dict:
            listening = await asyncio.start_server(lambda r, w: w.close(), "127.0.0.1", 0, ssl=server)
            port = listening.sockets[0].getsockname()[1]
            try:
                _, writer = await asyncio.open_connection("127.0.0.1", port, ssl=trust, server_hostname=host)
                certificate = writer.get_extra_info("peercert")
                writer.close()
                return certificate
            finally:
                listening.close()

        return asyncio.run(run())

    def bottle_trusts(self, bottle: str = "b", hosts=HOSTS) -> ssl.SSLContext:
        return ssl.create_default_context(cadata=ca.certificate(bottle, hosts))


class GenerateTest(CATestCase):
    def test_generated_once_and_kept(self) -> None:
        first = ca.certificate("b", HOSTS)
        self.assertTrue(first.startswith("-----BEGIN CERTIFICATE-----"))
        self.assertEqual(ca.certificate("b", reversed(HOSTS)), first)

    def test_only_the_owner_can_read_the_keys(self) -> None:
        ca.certificate("b", HOSTS)
        directory = ca.ca_dir("b", HOSTS)
        for path in (directory, directory.parent, directory.parent.parent):
            self.assertEqual(path.stat().st_mode & 0o777, 0o700, path)
        for key in ("ca.key", "leaf.key"):
            self.assertEqual((directory / key).stat().st_mode & 0o777, 0o600, key)

    def test_two_starting_at_once_agree_on_one(self) -> None:
        results = []
        threads = [threading.Thread(target=lambda: results.append(ca.certificate("b", HOSTS))) for _ in range(4)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual(len(set(results)), 1)
        self.assertEqual([p.name for p in ca.hosts_dir("b").iterdir()], [ca.ca_dir("b", HOSTS).name])

    def test_each_bottle_has_its_own(self) -> None:
        self.assertNotEqual(ca.certificate("b", HOSTS), ca.certificate("c", HOSTS))

    def test_new_hosts_get_a_new_ca_and_the_old_one_goes(self) -> None:
        first = ca.certificate("b", HOSTS)
        self.assertNotEqual(ca.certificate("b", ("ci.test",)), first)
        self.assertEqual([p.name for p in ca.hosts_dir("b").iterdir()], [ca.ca_dir("b", ("ci.test",)).name])

    def test_no_hosts_no_ca(self) -> None:
        ca.certificate("b", HOSTS)
        self.assertIsNone(ca.certificate("b", ()))
        self.assertEqual(list(ca.hosts_dir("b").iterdir()), [])

    def test_a_deleted_bottles_cas_go_with_it(self) -> None:
        ca.certificate("b", HOSTS)
        ca.forget("b")
        self.assertFalse(ca.hosts_dir("b").exists())
        ca.forget("b")  # and again, with nothing there

    def test_the_ca_every_bottle_shared_is_removed(self) -> None:
        shared = self.home / "ca"
        shared.mkdir(parents=True)
        for name in ("ca.crt", "ca.key", "leaf.key"):
            (shared / name).write_text("from before CAs were per bottle")
        ca.certificate("b", HOSTS)
        self.assertEqual([p.name for p in shared.iterdir()], ["b"])

    def test_not_a_bottles_name(self) -> None:
        for bottle in ("", "..", ".hidden", "a/b"):
            with self.subTest(bottle), self.assertRaises(BottleError):
                ca.certificate(bottle, HOSTS)

    def test_hosts_a_constraint_cant_say(self) -> None:
        for host in ("ci-*.example.com", "ci?.example.com", "[ab].example.com", "*", "*.", "a b", "a..b",
                     "fe80::1%eth0", "fe80::1%x\nsubjectAltName = DNS:elsewhere.test", "example.com:443"):
            with self.subTest(host), self.assertRaises(BottleError):
                ca.certificate("b", (host,))


class ConstraintTest(CATestCase):
    """What a bottle trusting its CA accepts: the CA's hosts, and nothing else, whoever signs it."""

    def minted_anyway(self, host: str, hosts=HOSTS) -> ssl.SSLContext:
        """A leaf for `host` signed with the CA's own key, as a bottled that didn't check would make."""
        ca.certificate("b", hosts)
        return ca._mint(ca.ca_dir("b", hosts), host)

    def test_the_cas_hosts_are_accepted(self) -> None:
        trust = self.bottle_trusts()
        for host in HOSTS:
            with self.subTest(host):
                self.handshake(ca.server_context("b", HOSTS, host), host, trust)

    def test_any_other_name_is_refused(self) -> None:
        trust = self.bottle_trusts()
        for host in ("example.com", "github.com.evil.test", "hubgithub.com"):
            with self.subTest(host), self.assertRaises(ssl.SSLCertVerificationError):
                self.handshake(self.minted_anyway(host), host, trust)

    def test_any_other_address_is_refused(self) -> None:
        trust = self.bottle_trusts()
        for host in ("10.1.2.4", "1.1.1.1", "fd00::1"):
            with self.subTest(host), self.assertRaises(ssl.SSLCertVerificationError):
                self.handshake(self.minted_anyway(host), host, trust)

    def test_no_addresses_named_means_none_at_all(self) -> None:
        hosts = ("github.com",)
        trust = self.bottle_trusts(hosts=hosts)
        for host in ("10.1.2.3", "::1"):
            with self.subTest(host), self.assertRaises(ssl.SSLCertVerificationError):
                self.handshake(self.minted_anyway(host, hosts), host, trust)

    def test_only_addresses_named_means_no_names_at_all(self) -> None:
        hosts = ("10.1.2.3",)
        trust = self.bottle_trusts(hosts=hosts)
        self.handshake(ca.server_context("b", hosts, "10.1.2.3"), "10.1.2.3", trust)
        with self.assertRaises(ssl.SSLCertVerificationError):
            self.handshake(self.minted_anyway("github.com", hosts), "github.com", trust)

    def test_a_wildcard_is_the_names_below_it(self) -> None:
        hosts = ("*.example.com",)
        trust = self.bottle_trusts(hosts=hosts)
        self.handshake(ca.server_context("b", hosts, "ci.example.com"), "ci.example.com", trust)
        self.handshake(ca.server_context("b", hosts, "eu.ci.example.com"), "eu.ci.example.com", trust)
        with self.assertRaises(ssl.SSLCertVerificationError):
            self.handshake(self.minted_anyway("example.com", hosts), "example.com", trust)

    def test_another_bottles_ca_is_no_good(self) -> None:
        trust, _ = self.bottle_trusts("b"), ca.certificate("c", HOSTS)
        with self.assertRaises(ssl.SSLCertVerificationError):
            self.handshake(ca.server_context("c", HOSTS, "github.com"), "github.com", trust)

    def test_nobody_else_accepts_it(self) -> None:
        ca.certificate("b", HOSTS)
        with self.assertRaises(ssl.SSLCertVerificationError):
            self.handshake(ca.server_context("b", HOSTS, "github.com"), "github.com", ssl.create_default_context())


class LeafTest(CATestCase):
    def setUp(self) -> None:
        super().setUp()
        self.trust = self.bottle_trusts()

    def test_the_host_it_was_asked_for(self) -> None:
        certificate = self.handshake(ca.server_context("b", HOSTS, "api.github.com"), "api.github.com", self.trust)
        self.assertEqual(certificate["subjectAltName"], (("DNS", "api.github.com"),))

    def test_an_ip_address(self) -> None:
        certificate = self.handshake(ca.server_context("b", HOSTS, "10.1.2.3"), "10.1.2.3", self.trust)
        self.assertEqual(certificate["subjectAltName"], (("IP Address", "10.1.2.3"),))

    def test_no_common_name(self) -> None:
        # GnuTLS (git's TLS in a bottle) checks the CN of a certificate with no
        # DNS name against the CA's DNS constraints, and OpenSSL does when it
        # looks like a host name: either would refuse an address's leaf.
        certificate = self.handshake(ca.server_context("b", HOSTS, "10.1.2.3"), "10.1.2.3", self.trust)
        self.assertEqual(certificate["subject"], ((("organizationName", "bottle egress"),),))

    def test_an_ipv6_address(self) -> None:
        hosts = ("fd00::1",)
        trust = self.bottle_trusts(hosts=hosts)
        certificate = self.handshake(ca.server_context("b", hosts, "fd00::1"), "fd00::1", trust)
        self.assertEqual(certificate["subjectAltName"], (("IP Address", "FD00:0:0:0:0:0:0:1"),))

    def test_minted_once_per_host(self) -> None:
        self.assertIs(ca.server_context("b", HOSTS, "GitHub.COM"), ca.server_context("b", HOSTS, "github.com"))
        self.assertIsNot(ca.server_context("b", HOSTS, "github.com"), ca.server_context("b", HOSTS, "api.github.com"))

    def test_minted_again_when_its_time_is_up(self) -> None:
        first = ca.server_context("b", HOSTS, "github.com")
        with mock.patch.object(ca.time, "time", return_value=ca.time.time() + ca.LEAF_REFRESH + 1):
            self.assertIsNot(ca.server_context("b", HOSTS, "github.com"), first)

    def test_minted_again_for_a_ca_made_again(self) -> None:
        # A bottle deleted and made again with the same name and hosts gets a new CA in the same place.
        first = ca.server_context("b", HOSTS, "github.com")
        ca.forget("b")
        trust = self.bottle_trusts()
        self.assertIsNot(ca.server_context("b", HOSTS, "github.com"), first)
        self.handshake(ca.server_context("b", HOSTS, "github.com"), "github.com", trust)

    def test_only_so_many_are_kept(self) -> None:
        hosts = ("*.example.com",)
        ca.certificate("b", hosts)
        with mock.patch.object(ca, "LEAVES_KEPT", 2), mock.patch.object(ca, "_mint", side_effect=lambda d, h: object()):
            first = ca.server_context("b", hosts, "a.example.com")
            ca.server_context("b", hosts, "b.example.com")
            ca.server_context("b", hosts, "a.example.com")  # used again, so b goes first
            ca.server_context("b", hosts, "c.example.com")
            self.assertEqual({host for *_, host in ca._leaves}, {"a.example.com", "c.example.com"})
            self.assertIs(ca.server_context("b", hosts, "a.example.com"), first)

    def test_only_for_the_cas_hosts(self) -> None:
        for host in ("example.com", "x.github.com", "10.1.2.4"):
            with self.subTest(host), self.assertRaises(BottleError):
                ca.server_context("b", HOSTS, host)

    def test_not_without_a_ca(self) -> None:
        # bottled never makes one: the bottle wouldn't trust it.
        with self.assertRaisesRegex(BottleError, "made when the bottle starts"):
            ca.server_context("c", HOSTS, "github.com")
        self.assertFalse(ca.hosts_dir("c").exists())

    def test_not_a_host_name(self) -> None:
        hosts = ("*.example.com",)
        ca.certificate("b", hosts)
        for host in ("a b.example.com", "x/.example.com", "%.example.com", "\n.example.com"):
            with self.subTest(host), self.assertRaises(BottleError):
                ca.server_context("b", hosts, host)


if __name__ == "__main__":
    unittest.main()
