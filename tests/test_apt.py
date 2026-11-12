import unittest

from bottle import apt, cache


class CachedTest(unittest.TestCase):
    """What bottled keeps of an apt repository: its packages, the .deb files under its URI, on its scheme's port."""

    def test_a_repositorys_packages_and_only_those(self) -> None:
        kept = apt.Repository("https://downloads.example/claude/apt/stable").cached()
        self.assertEqual((kept.host, kept.ports), ("downloads.example", (cache.HTTPS_PORT,)))
        self.assertFalse(kept.excludes("/claude/apt/stable/pool/main/c/claude_2.1_amd64.deb"))
        for path in ("/claude/apt/stable/dists/stable/InRelease", "/claude/apt/stable/dists/stable/main/binary-amd64/Packages.gz",
                     "/other/pool/x.deb"):
            with self.subTest(path):
                self.assertTrue(kept.excludes(path))

    def test_plain_http_on_its_own_port(self) -> None:
        self.assertEqual(apt.Repository("http://deb.debian.org/debian").cached().ports, (cache.HTTP_PORT,))

    def test_a_hosts_repositories_are_one_entry(self) -> None:
        [debian] = apt.cached(apt.DEBIAN)
        self.assertEqual(debian.host, "deb.debian.org")
        self.assertFalse(debian.excludes("/debian/pool/main/g/git/git_2.47.3-0+deb13u1_amd64.deb"))
        self.assertFalse(debian.excludes("/debian-security/pool/updates/main/o/openssl/libssl3_3.5.7_amd64.deb"))
        self.assertTrue(debian.excludes("/debian/dists/trixie/InRelease"))

    def test_every_bottle_keeps_debians(self) -> None:
        self.assertIn(apt.cached(apt.DEBIAN)[0], cache.cached_for([]))


if __name__ == "__main__":
    unittest.main()
