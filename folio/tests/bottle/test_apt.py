import unittest

from folio.bottle import cache, features


class CachedTest(unittest.TestCase):
    """What's kept of apt repositories, as folio-cli has it: their packages, the .deb files under their URIs."""

    def test_every_bottle_keeps_debians(self) -> None:
        [debian] = [c for c in cache.cached_for([]) if c.host == "deb.debian.org"]
        self.assertEqual(sorted(debian.include), ["/debian-security/*.deb", "/debian/*.deb"])
        self.assertEqual((debian.ports, debian.immutable), ((cache.HTTP_PORT,), True))

    def test_a_build_keeps_apt_repositories_alone(self) -> None:
        # Maven's hosts are the jvm feature's to cache in a bottle; in a build only apt trusts the builds' CA.
        hosts = {c.host for c in cache.cached_for(["jvm"])}
        self.assertIn("repo.maven.apache.org", hosts)
        built = {c.host for c in features.build_cached()}
        self.assertNotIn("repo.maven.apache.org", built)
        self.assertIn("deb.debian.org", built)


if __name__ == "__main__":
    unittest.main()
