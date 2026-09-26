import json
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


class WrapTest(GitTestCase):
    def test_registers_repo(self) -> None:
        repo = self.make_repo()

        result = repos.wrap(repo)

        self.assertTrue(result.created)
        self.assertEqual(result.repo, repos.Repo("project", repo))
        self.assertEqual(repos.load(), {"project": repos.Repo("project", repo)})

    def test_registry_file(self) -> None:
        repo = self.make_repo()
        repos.wrap(repo)
        data = json.loads((self.bottle_home / "repos.json").read_text())
        self.assertEqual(data, {"version": 1, "repos": {"project": {"path": str(repo)}}})

    def test_copies_nothing(self) -> None:
        repos.wrap(self.make_repo())
        self.assertEqual([p.name for p in self.bottle_home.iterdir()], ["repos.json"])

    def test_name_from_origin(self) -> None:
        repo = self.make_repo("checkout", origin="git@github.com:example/project.git")
        self.assertEqual(repos.wrap(repo).repo.name, "project")

    def test_name_falls_back_to_directory(self) -> None:
        repo = self.make_repo("my-tool")
        self.assertEqual(repos.wrap(repo).repo.name, "my-tool")

    def test_explicit_name(self) -> None:
        repo = self.make_repo(origin="git@github.com:example/project.git")
        self.assertEqual(repos.wrap(repo, "custom").repo.name, "custom")

    def test_rejects_subdirectory(self) -> None:
        repo = self.make_repo()
        (repo / "sub").mkdir()
        with self.assertRaisesRegex(BottleError, f"wrap its root instead: {repo}$"):
            repos.wrap(repo / "sub")

    def test_accepts_symlink_to_root(self) -> None:
        repo = self.make_repo()
        link = self.tmp / "link"
        link.symlink_to(repo)
        self.assertEqual(repos.wrap(link).repo.path, repo)

    def test_rewrap_is_a_no_op(self) -> None:
        repo = self.make_repo()
        repos.wrap(repo)
        before = (self.bottle_home / "repos.json").read_text()

        result = repos.wrap(repo)

        self.assertFalse(result.created)
        self.assertEqual((self.bottle_home / "repos.json").read_text(), before)

    def test_keeps_other_repos(self) -> None:
        a, b = self.make_repo("a"), self.make_repo("b")
        repos.wrap(a)
        repos.wrap(b)
        self.assertEqual(set(repos.load()), {"a", "b"})

    def test_name_taken_by_other_repo(self) -> None:
        repos.wrap(self.make_repo("a"), "shared")
        with self.assertRaisesRegex(BottleError, "already wraps"):
            repos.wrap(self.make_repo("b"), "shared")

    def test_repo_already_wrapped_under_other_name(self) -> None:
        repo = self.make_repo()
        repos.wrap(repo, "first")
        with self.assertRaisesRegex(BottleError, "already wrapped as 'first'"):
            repos.wrap(repo, "second")

    def test_invalid_name(self) -> None:
        with self.assertRaisesRegex(BottleError, "invalid name"):
            repos.wrap(self.make_repo(), "bad name")

    def test_missing_directory(self) -> None:
        with self.assertRaisesRegex(BottleError, "not a directory"):
            repos.wrap(self.tmp / "nope")

    def test_not_a_repo(self) -> None:
        plain = self.tmp / "plain"
        plain.mkdir()
        with self.assertRaisesRegex(BottleError, "is not a git repo"):
            repos.wrap(plain)

    def test_load_without_registry(self) -> None:
        self.assertEqual(repos.load(), {})


if __name__ == "__main__":
    unittest.main()
