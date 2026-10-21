import unittest

from bottle import repos
from bottle.errors import BottleError
from tests.support import GitTestCase


class NameFromUrlTest(unittest.TestCase):
    def test_https(self) -> None:
        self.assertEqual(repos.name_from_url("https://github.com/example/project.git"), "project")
        self.assertEqual(repos.name_from_url("https://github.com/example/project"), "project")

    def test_scp_style(self) -> None:
        self.assertEqual(repos.name_from_url("git@github.com:example/project.git"), "project")
        self.assertEqual(repos.name_from_url("host:project.git"), "project")

    def test_trailing_slash(self) -> None:
        self.assertEqual(repos.name_from_url("https://example.com/org/repo/"), "repo")

    def test_local_path(self) -> None:
        self.assertEqual(repos.name_from_url("/srv/git/tools.git"), "tools")


class DeriveNameTest(GitTestCase):
    def test_from_origin(self) -> None:
        repo = self.make_repo("checkout", origin="git@github.com:example/project.git")
        self.assertEqual(repos.derive_name(repo), "project")

    def test_falls_back_to_the_directory(self) -> None:
        self.assertEqual(repos.derive_name(self.make_repo("my-tool")), "my-tool")


class ToplevelTest(GitTestCase):
    """What `--git-repo` accepts: a repo's root, and nothing else."""

    def test_a_repos_root(self) -> None:
        repo = self.make_repo()
        self.assertEqual(repos.toplevel(repo), repo)

    def test_through_a_symlink(self) -> None:
        repo = self.make_repo()
        link = self.tmp / "link"
        link.symlink_to(repo)
        self.assertEqual(repos.toplevel(link), repo)

    def test_refuses_a_subdirectory(self) -> None:
        repo = self.make_repo()
        (repo / "sub").mkdir()
        with self.assertRaisesRegex(BottleError, f"give its root instead: {repo}$"):
            repos.toplevel(repo / "sub")

    def test_refuses_what_isnt_a_repo(self) -> None:
        plain = self.tmp / "plain"
        plain.mkdir()
        for path, message in ((self.tmp / "nope", "not a directory"), (plain, "is not a git repo")):
            with self.subTest(path), self.assertRaisesRegex(BottleError, message):
                repos.toplevel(path)
