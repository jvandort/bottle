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


class AddTest(GitTestCase):
    def test_registers_repo(self) -> None:
        repo = self.make_repo()

        result = repos.add(repo)

        self.assertTrue(result.created)
        self.assertEqual(result.repo, repos.Repo("project", repo))
        self.assertEqual(repos.load(), {"project": repos.Repo("project", repo)})

    def test_registry_file(self) -> None:
        repo = self.make_repo()
        repos.add(repo)
        data = json.loads((self.bottle_home / "repos.json").read_text())
        self.assertEqual(data, {"version": 1, "repos": {"project": {"path": str(repo), "features": []}}})

    def test_copies_nothing(self) -> None:
        repos.add(self.make_repo())
        self.assertEqual([p.name for p in self.bottle_home.iterdir()], ["repos.json"])

    def test_name_from_origin(self) -> None:
        repo = self.make_repo("checkout", origin="git@github.com:example/project.git")
        self.assertEqual(repos.add(repo).repo.name, "project")

    def test_name_falls_back_to_directory(self) -> None:
        repo = self.make_repo("my-tool")
        self.assertEqual(repos.add(repo).repo.name, "my-tool")

    def test_explicit_name(self) -> None:
        repo = self.make_repo(origin="git@github.com:example/project.git")
        self.assertEqual(repos.add(repo, "custom").repo.name, "custom")

    def test_rejects_subdirectory(self) -> None:
        repo = self.make_repo()
        (repo / "sub").mkdir()
        with self.assertRaisesRegex(BottleError, f"add its root instead: {repo}$"):
            repos.add(repo / "sub")

    def test_accepts_symlink_to_root(self) -> None:
        repo = self.make_repo()
        link = self.tmp / "link"
        link.symlink_to(repo)
        self.assertEqual(repos.add(link).repo.path, repo)

    def test_rewrap_is_a_no_op(self) -> None:
        repo = self.make_repo()
        repos.add(repo)
        before = (self.bottle_home / "repos.json").read_text()

        result = repos.add(repo)

        self.assertFalse(result.created)
        self.assertEqual((self.bottle_home / "repos.json").read_text(), before)

    def test_keeps_other_repos(self) -> None:
        a, b = self.make_repo("a"), self.make_repo("b")
        repos.add(a)
        repos.add(b)
        self.assertEqual(set(repos.load()), {"a", "b"})

    def test_name_taken_by_other_repo(self) -> None:
        repos.add(self.make_repo("a"), "shared")
        with self.assertRaisesRegex(BottleError, "is already"):
            repos.add(self.make_repo("b"), "shared")

    def test_repo_already_wrapped_under_other_name(self) -> None:
        repo = self.make_repo()
        repos.add(repo, "first")
        with self.assertRaisesRegex(BottleError, "already added as 'first'"):
            repos.add(repo, "second")

    def test_invalid_name(self) -> None:
        with self.assertRaisesRegex(BottleError, "invalid name"):
            repos.add(self.make_repo(), "bad name")

    def test_missing_directory(self) -> None:
        with self.assertRaisesRegex(BottleError, "not a directory"):
            repos.add(self.tmp / "nope")

    def test_not_a_repo(self) -> None:
        plain = self.tmp / "plain"
        plain.mkdir()
        with self.assertRaisesRegex(BottleError, "is not a git repo"):
            repos.add(plain)

    def test_features(self) -> None:
        repo = self.make_repo()
        added = repos.add(repo, features=["tools", "jvm:version=25", "jvm:version=25", "claude"]).repo
        # Canonical (the default version drops out), deduplicated, in the order given.
        self.assertEqual(added.features, ("tools", "jvm", "claude"))
        self.assertEqual(repos.get("project").features, ("tools", "jvm", "claude"))

    def test_features_are_validated(self) -> None:
        repo = self.make_repo()
        with self.assertRaisesRegex(BottleError, "no feature named 'nope'"):
            repos.add(repo, features=["nope"])
        with self.assertRaisesRegex(BottleError, "has no option 'vesion'"):
            repos.add(repo, features=["jvm:vesion=17"])
        with self.assertRaisesRegex(BottleError, "given twice with different options"):
            repos.add(repo, features=["jvm:version=17", "jvm:version=21"])
        self.assertEqual(repos.load(), {})

    def test_adding_again_with_other_features_points_to_set(self) -> None:
        repo = self.make_repo()
        repos.add(repo, features=["tools"])
        self.assertFalse(repos.add(repo).created)  # no features given: a no-op
        self.assertFalse(repos.add(repo, features=["tools"]).created)
        with self.assertRaisesRegex(BottleError, "change its features with `bottle repo set project`"):
            repos.add(repo, features=["claude"])

    def test_set_replaces_the_settings(self) -> None:
        repo = self.make_repo()
        repos.add(repo, features=["tools", "claude"])
        self.assertEqual(repos.set_settings("project", ["jvm:version=17"]).features, ("jvm:version=17",))
        self.assertEqual(repos.set_settings("project", []).features, ())
        self.assertEqual(repos.get("project").path, repo)

    def test_set_unknown_repo(self) -> None:
        with self.assertRaisesRegex(BottleError, "no repo named 'nope'"):
            repos.set_settings("nope", [])

    def test_hand_edited_registry_without_features(self) -> None:
        repo = self.make_repo()
        (self.bottle_home).mkdir(parents=True, exist_ok=True)
        (self.bottle_home / "repos.json").write_text(json.dumps({"version": 1, "repos": {"p": {"path": str(repo)}}}))
        self.assertEqual(repos.get("p").features, ())

    def test_load_without_registry(self) -> None:
        self.assertEqual(repos.load(), {})


if __name__ == "__main__":
    unittest.main()
