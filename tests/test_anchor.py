import unittest
from unittest import mock

from bottle import anchor, runtime
from tests.support import GitTestCase


class FakeRuntime:
    def __init__(self) -> None:
        self.networks = {"default", "bottle-a", "bottle-b"}
        self.containers: dict[str, runtime.ContainerInfo] = {}
        self.calls: list[str] = []

    def idle_run(self, name, image, networks, labels=None):
        # The old anchor must still be there: no network goes without one.
        self.calls.append(f"run {sorted(networks)} alongside {sorted(self.containers)}")
        self.containers[name] = runtime.ContainerInfo("running", dict(labels or {}), list(networks))

    def container_delete(self, name, owner=None):
        self.calls.append(f"delete {name}")
        del self.containers[name]

    def patch(self, test: unittest.TestCase) -> None:
        for name, value in (
            ("networks", lambda: set(self.networks)),
            ("containers", lambda: dict(self.containers)),
            ("idle_run", self.idle_run),
            ("container_delete", self.container_delete),
        ):
            patcher = mock.patch.object(anchor.runtime, name, value)
            patcher.start()
            test.addCleanup(patcher.stop)

    def anchors(self) -> dict[str, list[str]]:
        return {name: sorted(info.networks) for name, info in self.containers.items() if anchor.LABEL in info.labels}


class EnsureTest(GitTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.fake = FakeRuntime()
        self.fake.patch(self)

    def test_anchors_the_networks_and_the_default_one(self) -> None:
        anchor.ensure({"bottle-a"})
        self.assertEqual(list(self.fake.anchors().values()), [["bottle-a", "default"]])

    def test_already_anchored_is_a_no_op(self) -> None:
        anchor.ensure({"bottle-a"})
        self.fake.calls.clear()
        anchor.ensure({"bottle-a"})
        self.assertEqual(self.fake.calls, [])

    def test_a_new_network_replaces_the_anchor_after_starting_the_new_one(self) -> None:
        anchor.ensure({"bottle-a"})
        [old] = self.fake.anchors()
        anchor.ensure({"bottle-a", "bottle-b"})
        self.assertEqual(self.fake.calls[1:], [f"run ['bottle-a', 'bottle-b', 'default'] alongside ['{old}']", f"delete {old}"])
        self.assertEqual(list(self.fake.anchors().values()), [["bottle-a", "bottle-b", "default"]])

    def test_networks_that_dont_exist_are_left_out(self) -> None:
        anchor.ensure({"bottle-a", "bottle-gone"})
        self.assertEqual(list(self.fake.anchors().values()), [["bottle-a", "default"]])

    def test_no_bottle_networks_means_no_anchor(self) -> None:
        anchor.ensure({"bottle-a"})
        anchor.ensure(set())
        self.assertEqual(self.fake.anchors(), {})

    def test_a_stopped_anchor_is_replaced(self) -> None:
        anchor.ensure({"bottle-a"})
        [old] = self.fake.anchors()
        self.fake.containers[old] = runtime.ContainerInfo("stopped", {anchor.LABEL: "1"}, ["bottle-a", "default"])
        anchor.ensure({"bottle-a"})
        self.assertNotIn(old, self.fake.anchors())
        self.assertEqual(list(self.fake.anchors().values()), [["bottle-a", "default"]])

    def test_leaves_other_containers_alone(self) -> None:
        self.fake.containers["bottle-a"] = runtime.ContainerInfo("running", {}, ["bottle-a"])
        self.fake.containers[f"{anchor._prefix()}unlabelled"] = runtime.ContainerInfo("running", {}, ["bottle-a"])
        anchor.ensure(set())
        self.assertEqual(sorted(self.fake.containers), sorted(["bottle-a", f"{anchor._prefix()}unlabelled"]))
