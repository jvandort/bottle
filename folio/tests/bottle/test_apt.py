import unittest

from folio.bottle import apt, cache, features


class CachedTest(unittest.TestCase):
    """What's kept of an apt repository: its packages, the .deb files under its URI, on its scheme's port."""

    def test_a_repositorys_packages_and_only_those(self) -> None:
        kept = apt.Repository("https://downloads.example/claude/apt/stable").cached()
        self.assertEqual((kept.host, kept.ports), ("downloads.example", (cache.HTTPS_PORT,)))
        # Matched by bottle-cli as fnmatch would (agent-client's policy): its indexes change, and aren't kept.
        self.assertEqual((kept.include, kept.immutable), (("/claude/apt/stable/*.deb",), True))

    def test_plain_http_on_its_own_port(self) -> None:
        self.assertEqual(apt.Repository("http://deb.debian.org/debian").cached().ports, (cache.HTTP_PORT,))

    def test_a_hosts_repositories_are_one_entry(self) -> None:
        [debian] = apt.cached(apt.DEBIAN)
        self.assertEqual(debian.host, "deb.debian.org")
        self.assertEqual(sorted(debian.include), ["/debian-security/*.deb", "/debian/*.deb"])

    def test_every_bottle_keeps_debians(self) -> None:
        self.assertIn(apt.cached(apt.DEBIAN)[0], cache.cached_for([]))

    def test_a_build_keeps_apt_repositories_alone(self) -> None:
        # Maven's hosts are the jvm feature's to cache in a bottle; in a build only apt trusts the builds' CA.
        hosts = {c.host for c in cache.cached_for(["jvm"])}
        self.assertIn("repo.maven.apache.org", hosts)
        built = {c.host for c in features.build_cached()}
        self.assertNotIn("repo.maven.apache.org", built)
        self.assertIn("deb.debian.org", built)


if __name__ == "__main__":
    unittest.main()
