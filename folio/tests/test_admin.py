from folio import repos
from tests.support import FolioTestCase


class BottleTest(FolioTestCase):
    def test_a_threads_bottle_goes_with_its_thread(self) -> None:
        self.ok("new", "fix-it")
        bottle = f"{repos.current(self.repo).key}-fix-it"
        result = self.folio("bottle", "rm", bottle)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"{bottle} is thread {self.repo.name}/fix-it's: it goes with the thread", result.stderr)
        self.assertIn(bottle, self.fake()["bottles"])
