import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from bottle import daemon, images
from bottle.errors import BottleError
from tests.support import GitTestCase


class TagTest(unittest.TestCase):
    def test_images_are_named_in_full(self) -> None:
        self.assertEqual(images.tag("base"), "localhost/bottle/base:latest")


class DependencyTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.containers = Path(tmp.name)
        patcher = mock.patch.object(images, "IMAGES", self.containers)
        patcher.start()
        self.addCleanup(patcher.stop)

    def image(self, name: str, **spec) -> None:
        (self.containers / name).mkdir()
        (self.containers / name / "image.json").write_text(json.dumps(spec))
        (self.containers / name / "install.sh").write_text("#!/bin/sh\n")

    def test_real_images(self) -> None:
        with mock.patch.object(images, "IMAGES", Path(images.__file__).resolve().parent.parent / "containers" / "images"):
            self.assertIn("base", images.available())
            self.assertEqual(images.dependencies("base"), [])
            self.assertEqual(images.recipe("base")["pull"], "debian:trixie-slim")

    def test_from(self) -> None:
        self.image("base", pull="debian:13")
        self.image("a", **{"from": "base"})
        self.assertEqual(images.dependencies("a"), ["base"])
        self.assertEqual(images.dependencies("base"), [])

    def test_order_is_dependencies_first(self) -> None:
        self.image("base", pull="debian:13")
        self.image("tools", **{"from": "base"})
        self.image("agent", **{"from": "tools"})
        self.assertEqual(images.build_order("agent"), ["base", "tools", "agent"])

    def test_cycle(self) -> None:
        self.image("a", **{"from": "b"})
        self.image("b", **{"from": "a"})
        with self.assertRaisesRegex(BottleError, "cycle: a -> b -> a"):
            images.build_order("a")

    def test_missing_dependency(self) -> None:
        self.image("a", **{"from": "ghost"})
        with self.assertRaisesRegex(BottleError, "a is built from ghost, but there's no containers/images/ghost"):
            images.build_order("a")

    def test_a_definition_is_checked(self) -> None:
        for spec, error in (({}, "needs pull"), ({"pull": "a", "from": "b"}, "not both"),
                            ({"pull": "a", "run": "x"}, "unsupported: run"), ({"pull": "a", "env": {"A": 1}}, "env"),
                            ({"pull": "a", "cmd": "sh"}, "cmd must be a list")):
            with self.subTest(spec=spec):
                (self.containers / "x").mkdir(exist_ok=True)
                (self.containers / "x" / "image.json").write_text(json.dumps(spec))
                (self.containers / "x" / "install.sh").write_text("")
                with self.assertRaisesRegex(BottleError, error):
                    images.definition("x")

    def test_a_recipe_runs_install_sh_where_its_directory_is_mounted(self) -> None:
        self.image("base", pull="debian:13", env={"LANG": "C.UTF-8"}, entrypoint=["tini"], cmd=["sshd"],
                   labels={"title": "base"})
        self.image("tools", **{"from": "base"})
        (self.containers / "tools" / "install.sh").write_text("#!/bin/bash -e\n")
        self.assertEqual(images.recipe("base"), {
            "pull": "debian:13", "entrypoint": ["tini"], "cmd": ["sshd"], "labels": {"title": "base"},
            "steps": [{"env": {"LANG": "C.UTF-8"}},
                      {"run": ["/bin/sh", "./install.sh"], "env": images.STEP_ENV, "cwd": images.MOUNT}]})
        # By its #! line: a read-only mount can't be made executable.
        self.assertEqual(images.recipe("tools"), {
            "from": "localhost/bottle/base:latest", "labels": {},
            "steps": [{"run": ["/bin/bash", "-e", "./install.sh"], "env": images.STEP_ENV, "cwd": images.MOUNT}]})

    def test_ensure_built_builds_whats_missing_or_stale(self) -> None:
        self.image("base", pull="debian:13")
        self.image("tools", **{"from": "base"})
        built = []
        for exists, expected in ((set(), ["base", "tools"]), ({"localhost/bottle/base:latest"}, ["tools"]),
                                 ({"localhost/bottle/base:latest", "localhost/bottle/tools:latest"}, [])):
            built.clear()
            with self.subTest(exists=exists), \
                    mock.patch.object(images, "is_current", side_effect=lambda t, inputs, runner=None: t in exists), \
                    mock.patch.object(images, "_build_one", side_effect=lambda n, no_cache=False, runner=None: built.append(n)), \
                    mock.patch("sys.stderr"):
                images.ensure_built("tools")
                self.assertEqual(built, expected)

    def test_build_rebuilds_the_image_but_not_built_dependencies(self) -> None:
        self.image("base", pull="debian:13")
        self.image("tools", **{"from": "base"})
        built = []
        with mock.patch.object(images, "is_current", return_value=True), \
                mock.patch.object(images, "_build_one", side_effect=lambda n, no_cache=False, runner=None: built.append((n, no_cache))):
            self.assertEqual(images.build("tools", no_cache=True), "localhost/bottle/tools:latest")
        self.assertEqual(built, [("tools", True)])


class StalenessTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        (self.dir / "install.sh").write_text("apt-get install a\n")

    def test_tree_hash_tracks_names_contents_and_the_executable_bit(self) -> None:
        before = images.tree_hash(self.dir)
        self.assertEqual(images.tree_hash(self.dir), before)
        (self.dir / "install.sh").write_text("apt-get install b\n")
        after_edit = images.tree_hash(self.dir)
        (self.dir / "install.sh").chmod(0o755)
        after_chmod = images.tree_hash(self.dir)
        (self.dir / "extra").write_text("")
        self.assertEqual(len({before, after_edit, after_chmod, images.tree_hash(self.dir)}), 4)

    def test_is_current_compares_the_label(self) -> None:
        with mock.patch.object(images, "built_labels", return_value={"bottle.inputs": "abc"}):
            self.assertTrue(images.is_current("localhost/bottle/base:latest", "abc"))
            self.assertFalse(images.is_current("localhost/bottle/base:latest", "def"))
        with mock.patch.object(images, "built_labels", return_value=None):
            self.assertFalse(images.is_current("localhost/bottle/base:latest", "abc"))  # missing
        with mock.patch.object(images, "built_labels", return_value={}):
            self.assertFalse(images.is_current("localhost/bottle/base:latest", "abc"))  # built before labels


class BuildImageTest(unittest.TestCase):
    def test_base_is_available(self) -> None:
        self.assertIn("base", images.available())

    def test_unknown_image(self) -> None:
        with mock.patch.object(images.prereqs, "ensure_cli") as ensure_cli, \
                self.assertRaisesRegex(BottleError, "no image named 'nope'; available: .*base"):
            images.build("nope")
        ensure_cli.assert_not_called()

    def test_an_image_is_built_from_its_directory(self) -> None:
        with mock.patch.object(images, "build_recipe") as build:
            self.assertEqual(images.build("base"), "localhost/bottle/base:latest")
        context, tag, inputs, recipe = build.call_args.args[:4]
        self.assertEqual((context, tag, inputs), (images.IMAGES / "base", "localhost/bottle/base:latest",
                                                   images.inputs_hash("base")))
        self.assertEqual(recipe, images.recipe("base"))


class StoreTest(GitTestCase):
    """Bottle's images: stages bottle-cli builds, each named for what it's made from, kept while one is used."""

    def setUp(self) -> None:
        super().setUp()
        self.calls: list[tuple] = []
        self.recipes: list[dict] = []
        self.build_status = 0
        self.context = self.tmp / "context"
        self.context.mkdir()
        (self.context / "install.sh").write_text("#!/bin/sh\n")

        def bind(bottle, *args, **kwargs):
            self.calls.append(("bind", bottle))
            return "127.0.0.1:47329", "token"

        for target, name, fake in (
            (daemon, "bind_decider", bind),
            (daemon, "release_egress", lambda bottle: self.calls.append(("release", bottle))),
            (images.subprocess, "run", self.cli),
            (images.prereqs, "ensure_cli", lambda: None),
        ):
            patcher = mock.patch.object(target, name, fake)
            patcher.start()
            self.addCleanup(patcher.stop)

    def cli(self, command, stdout=None, text=False, env=None, capture_output=False):
        """bottle-cli: a pull or a build makes a stage named for its arguments; gc deletes what it isn't told to keep."""
        args = command[1:]
        store = images.store()
        if args[0] == "gc":
            keep = {Path(k).name for k in args[2:]}
            for disk in store.glob("*.ext4"):
                if disk.name not in keep:
                    disk.unlink()
            return subprocess.CompletedProcess(command, 0, "", "")
        if args[0] == "pull":
            name = "pulled"
        else:
            self.calls.append(("build", args[3:], env.get("BOTTLE_DECIDER_TOKEN")))
            recipe = json.loads(Path(args[1]).read_text())
            self.recipes.append(recipe)
            if self.build_status:
                return subprocess.CompletedProcess(command, self.build_status, "", "")
            name = recipe["labels"]["bottle.inputs"][:8] + recipe["labels"].get("bottle.build", "")[:4]
        (store / f"{name}.ext4").write_text("")
        return subprocess.CompletedProcess(command, 0, f"{store}/below.ext4\n{store}/{name}.ext4\n", "")

    def build(self, tag: str, inputs: str, recipe: dict | None = None, no_cache: bool = False) -> Path:
        recipe = recipe or {"pull": "debian:13", "steps": [{"run": ["true"]}]}
        with mock.patch("sys.stderr"):
            return images.build_recipe(self.context, tag, inputs, recipe, no_cache)

    def test_a_build_runs_bound_like_a_bottle_and_released_after(self) -> None:
        self.build("localhost/bottle/base:latest", "aaaa" * 16)
        [bind, build, release] = self.calls
        self.assertEqual(bind[0], "bind")
        self.assertEqual(build, ("build", ["--decider", "127.0.0.1:47329"], "token"))
        self.assertEqual(release, ("release", bind[1]))

    def test_the_recipe_mounts_the_context_and_is_on_what_it_says(self) -> None:
        self.build("localhost/bottle/base:latest", "aaaa" * 16,
                   {"pull": "debian:13", "steps": [{"run": ["true"]}], "cmd": ["sshd"], "labels": {"title": "t"}})
        [recipe] = self.recipes
        self.assertEqual(recipe["from"], "pulled")
        self.assertEqual(recipe["mount"], {"source": str(self.context.resolve()), "target": images.MOUNT})
        self.assertEqual((recipe["steps"], recipe["cmd"]), ([{"run": ["true"]}], ["sshd"]))
        self.assertEqual(recipe["labels"], {"title": "t", "bottle.inputs": "aaaa" * 16})
        self.assertNotIn("entrypoint", recipe)  # what it's built on runs, then
        self.build("localhost/bottle/base:with-tools", "bbbb" * 16,
                   {"from": "localhost/bottle/base:latest", "steps": []})
        self.assertEqual(self.recipes[-1]["from"], "aaaaaaaa")

    def test_a_build_is_kept_under_its_tag(self) -> None:
        disk = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.assertEqual(disk.name, "aaaaaaaa.ext4")
        self.assertEqual(images.archive_of("localhost/bottle/base:latest"), disk)
        self.assertEqual(images.built_labels("localhost/bottle/base:latest"), {"bottle.inputs": "aaaa" * 16})

    def test_no_cache_makes_the_stage_anew(self) -> None:
        first = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        again = self.build("localhost/bottle/base:latest", "aaaa" * 16, no_cache=True)
        self.assertNotEqual(first, again)

    def test_on_an_image_that_isnt_built_is_an_error(self) -> None:
        with self.assertRaisesRegex(BottleError, "isn't built"):
            self.build("t", "inputs", {"from": "localhost/bottle/base:latest", "steps": []})

    def test_a_rebuild_leaves_the_stage_a_machine_was_made_from(self) -> None:
        old = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        new = self.build("localhost/bottle/base:latest", "bbbb" * 16)
        self.assertNotEqual(old, new)
        images.remove_unused({old.name})
        self.assertTrue(old.exists() and new.exists())
        images.remove_unused(set())
        self.assertFalse(old.exists())
        self.assertTrue(new.exists())

    def test_a_failed_build_is_an_error_keeps_nothing_and_releases_its_token(self) -> None:
        self.build_status = 1
        with self.assertRaisesRegex(BottleError, "bottle-cli build"):
            self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.assertIsNone(images.archive_of("localhost/bottle/base:latest"))
        self.assertEqual(self.calls[-1][0], "release")


if __name__ == "__main__":
    unittest.main()
