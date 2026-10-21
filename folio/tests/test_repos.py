from pathlib import Path

from folio import repos, store
from folio.errors import FolioError
from tests.support import FolioTestCase, git


class RepoTest(FolioTestCase):
    def test_key_is_the_name_and_a_hash_of_the_toplevel(self) -> None:
        repo = repos.current(self.repo)
        self.assertEqual((repo.name, repo.path), ("project", self.repo))
        self.assertEqual(repo.key, f"project-{repos.path_hash(self.repo)}")
        self.assertEqual(repo.directory, self.home / repo.key)

    def test_from_a_subdirectory_it_is_the_same_repo(self) -> None:
        (self.repo / "sub").mkdir()
        self.assertEqual(repos.current(self.repo / "sub"), repos.current(self.repo))

    def test_named_after_origin(self) -> None:
        for i, (url, name) in enumerate((("https://github.com/org/widget.git", "widget"),
                                         ("git@host:org/gadget.git", "gadget"), ("/srv/repos/odd name!.git", "odd-name"))):
            with self.subTest(url):
                repo = self.make_repo(f"r{i}")
                git("-C", str(repo), "remote", "add", "origin", url)
                self.assertEqual(repos.current(repo).name, name)

    def test_same_name_different_repos_different_keys(self) -> None:
        other = self.make_repo("elsewhere/project")
        self.assertNotEqual(repos.current(other).key, repos.current(self.repo).key)

    def test_not_in_a_repo(self) -> None:
        (self.tmp / "plain").mkdir()
        with self.assertRaisesRegex(FolioError, "not in a git repo"):
            repos.current(self.tmp / "plain")

    def test_known_repos_are_those_it_has_a_directory_for(self) -> None:
        self.assertEqual(repos.known(), [])
        repo = repos.current(self.repo)
        repo.save_spec({"features": ["tools"], "memory": "all"})
        self.assertEqual(repos.known(), [repo])
        self.assertEqual(store.read_json(repo.directory / "repo.json"), {"name": "project", "path": str(self.repo)})

    def test_merge_features(self) -> None:
        self.assertEqual(repos.merge_features(["tools", "jvm:version=17"], ["jvm:version=21", "claude"]),
                         ["tools", "jvm:version=21", "claude"])
