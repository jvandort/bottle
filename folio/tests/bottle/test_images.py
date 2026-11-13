import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from folio.bottle import cache, features, images
from folio.bottle.errors import BottleError
from tests.bottle.support import GitTestCase


class TagTest(unittest.TestCase):
    def test_images_are_named_in_full(self) -> None:
        self.assertEqual(images.tag("base"), "localhost/bottle/base:latest")


class RealBaseTest(unittest.TestCase):
    """The base image as this repo has it: the base feature on Debian."""

    def test_its_recipe(self) -> None:
        from folio.bottle import features

        recipe = features.base_recipe()
        self.assertEqual(recipe["pull"], images.DEBIAN)
        apt, install, env = recipe["steps"][:3]
        self.assertIn("install -y --no-install-recommends ca-certificates git openssh-server sudo tini", apt["run"][2])
        self.assertEqual(install["run"], ["/bin/sh", "./install.sh"])
        self.assertEqual(env["env"]["LANG"], "C.UTF-8")


class InterpreterTest(unittest.TestCase):
    def test_a_script_is_run_by_its_hash_bang_line(self) -> None:
        # A read-only mount can't be made executable.
        with tempfile.TemporaryDirectory() as tmp:
            script = Path(tmp) / "install.sh"
            for first, argv in (("#!/bin/bash -e", ["/bin/bash", "-e"]), ("#!/bin/sh", ["/bin/sh"]), ("set -e", ["/bin/sh"])):
                script.write_text(f"{first}\n")
                self.assertEqual(images.interpreter(script), [*argv, "./install.sh"])


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


class StoreTest(GitTestCase):
    """Bottle's images: stages bottle-cli builds, each named for what it's made from, kept while one is used."""

    def setUp(self) -> None:
        super().setUp()
        self.calls: list[tuple] = []
        self.recipes: list[dict] = []
        self.build_status = 0
        self.tags: dict[str, str] = {}
        self.labels: dict[str, dict] = {}
        self.context = self.tmp / "context"
        self.context.mkdir()
        (self.context / "install.sh").write_text("#!/bin/sh\n")

        for target, name, fake in (
            (images.krun.subprocess, "run", self.cli),
            (images.krun.prereqs, "ensure_cli", lambda: None),
            (features, "build_cached", lambda: [cache.Cached("deb.debian.org", ports=(80,))]),
        ):
            patcher = mock.patch.object(target, name, fake)
            patcher.start()
            self.addCleanup(patcher.stop)

    def cli(self, command, stdout=None, text=False, env=None, capture_output=False, input=None):
        """bottle-cli: an image build makes a stage named for its labels, on what it pulls or a tag's stage, and tags
        it; images lists the tags; gc deletes what no tag names and it isn't told to keep."""
        args = command[1:]
        store = self.tmp / "stages"
        store.mkdir(parents=True, exist_ok=True)
        done = lambda out="": subprocess.CompletedProcess(command, 0, out, "")
        if args[0] == "images":
            return done(json.dumps([{"tag": tag, "name": name, "disks": [str(store / f"{name}.ext4")],
                                     "labels": self.labels.get(name, {})}
                                    for tag, name in self.tags.items() if (store / f"{name}.ext4").exists()]))
        if args[:2] == ["image", "rm"]:
            del self.tags[args[2]]
            return done()
        if args[:2] == ["image", "tag"]:
            self.tags[args[3]] = args[2]
            return done()
        if args[0] == "gc":
            keep = {f"{name}.ext4" for name in self.tags.values()}
            for disk in store.glob("*.ext4"):
                if disk.name not in keep:
                    disk.unlink()
            return done()
        # image build RECIPE --tag TAG --network FILE
        self.calls.append(("build", args[3:-1], json.loads(Path(args[-1]).read_text())))
        recipe = json.loads(Path(args[2]).read_text())
        self.recipes.append(recipe)
        if self.build_status:
            return subprocess.CompletedProcess(command, self.build_status, "", "")
        name = recipe["labels"]["bottle.inputs"][:8] + recipe["labels"].get("bottle.build", "")[:4]
        (store / f"{name}.ext4").write_text("")
        self.tags[args[args.index("--tag") + 1]] = name
        self.labels[name] = recipe["labels"]
        return done(f"{store}/below.ext4\n{store}/{name}.ext4\n")

    def build(self, tag: str, inputs: str, recipe: dict | None = None) -> Path:
        recipe = recipe or {"pull": "debian:13", "steps": [{"run": ["true"]}]}
        with mock.patch("sys.stderr"):
            return images.build_recipe(self.context, tag, inputs, recipe)

    def test_a_build_reaches_the_public_network_its_apt_repositories_kept_and_nothing_routed(self) -> None:
        self.build("localhost/bottle/base:latest", "aaaa" * 16)
        [(_, args, network)] = self.calls
        self.assertEqual(args, ["--tag", "localhost/bottle/base:latest", "--network"])
        self.assertEqual(set(network), {"cache"})
        self.assertIn("deb.debian.org", [c["host"] for c in network["cache"]])

    def test_the_recipe_mounts_the_context_and_is_on_what_it_says(self) -> None:
        self.build("localhost/bottle/base:latest", "aaaa" * 16,
                   {"pull": "debian:13", "steps": [{"run": ["true"]}], "cmd": ["sshd"], "labels": {"title": "t"}})
        [recipe] = self.recipes
        self.assertEqual(recipe["pull"], "debian:13")  # bottle-cli's to pull
        self.assertEqual(recipe["mount"], {"source": str(self.context.resolve()), "target": images.MOUNT})
        self.assertEqual((recipe["steps"], recipe["cmd"]), ([{"run": ["true"]}], ["sshd"]))
        self.assertEqual(recipe["labels"], {"title": "t", "bottle.inputs": "aaaa" * 16})
        self.assertNotIn("entrypoint", recipe)  # what it's built on runs, then
        self.build("localhost/bottle/base:with-tools", "bbbb" * 16,
                   {"from": "localhost/bottle/base:latest", "steps": []})
        self.assertEqual(self.recipes[-1]["from"], "localhost/bottle/base:latest")  # by its tag

    def test_every_build_keeps_the_same_and_its_apt_step_asks_bottle_cli_for_the_builds_ca(self) -> None:
        recipe = {"pull": "debian:13", "features": ["claude"], "steps": [
            {"run": ["sh", "-c", "apt-get ..."], "env": {"HOME": "/root", images.BUILD_CA: ""}},
            {"run": ["./install.sh"], "env": {"HOME": "/root"}},
        ]}
        self.build("localhost/bottle/base:with-claude", "aaaa" * 16, recipe)
        [(_, _, network)] = self.calls
        self.assertEqual([c["host"] for c in network["cache"]], ["deb.debian.org"])   # features.build_cached
        apt_step, install = self.recipes[-1]["steps"]
        self.assertEqual(apt_step["env"], {"HOME": "/root", images.BUILD_CA: ""})   # bottle-cli fills it in
        self.assertNotIn("features", self.recipes[-1])  # bottle's, not bottle-cli's

    def test_a_build_is_kept_under_its_tag(self) -> None:
        disk = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.assertEqual(disk.name, "aaaaaaaa.ext4")
        self.assertEqual(images.built_labels("localhost/bottle/base:latest"), {"bottle.inputs": "aaaa" * 16})

    def test_on_an_image_that_isnt_built_is_an_error(self) -> None:
        with self.assertRaisesRegex(BottleError, "isn't built"):
            self.build("t", "inputs", {"from": "localhost/bottle/base:latest", "steps": []})

    def test_a_rebuild_leaves_the_stage_a_machine_was_made_from(self) -> None:
        old = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        new = self.build("localhost/bottle/base:latest", "bbbb" * 16)
        self.assertNotEqual(old, new)
        self.assertTrue(old.exists() and new.exists())
        images.remove_unused()   # no machine here was made from it
        self.assertFalse(old.exists())
        self.assertTrue(new.exists())

    def test_a_forgotten_image_is_untagged_and_its_stages_gone_once_unused(self) -> None:
        disk = self.build("localhost/bottle/base:latest", "aaaa" * 16)
        images.forget("localhost/bottle/base:latest")
        images.forget("localhost/bottle/base:latest")  # gone already: nothing to do
        self.assertEqual(images.built(), {})
        self.assertTrue(disk.exists())
        images.remove_unused()
        self.assertFalse(disk.exists())

    def test_a_failed_build_is_an_error_and_keeps_nothing(self) -> None:
        self.build_status = 1
        with self.assertRaisesRegex(BottleError, "bottle-cli image build"):
            self.build("localhost/bottle/base:latest", "aaaa" * 16)
        self.assertIsNone(images.built_labels("localhost/bottle/base:latest"))


if __name__ == "__main__":
    unittest.main()
