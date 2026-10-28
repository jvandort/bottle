import os
import unittest

from bottle import messages
from tests.support import GitTestCase


class AddressTest(GitTestCase):
    def test_an_address_reaches_its_target_from_its_holder(self) -> None:
        address = messages.address("broker", holder="foo")
        self.assertTrue(address.startswith("broker~"))
        self.assertEqual("broker", messages.target(address, holder="foo"))

    def test_an_address_is_nothing_from_any_other_bottle(self) -> None:
        self.assertIsNone(messages.target(messages.address("broker", holder="foo"), holder="bar"))

    def test_a_changed_target_or_signature_is_nothing(self) -> None:
        address = messages.address("broker", holder="foo")
        _, _, signature = address.partition("~")
        self.assertIsNone(messages.target(f"other~{signature}", holder="foo"))
        self.assertIsNone(messages.target(address[:-1] + ("a" if address[-1] != "a" else "b"), holder="foo"))
        self.assertIsNone(messages.target("~" + signature, holder="foo"))

    def test_addresses_last_as_long_as_the_key(self) -> None:
        address = messages.address("broker", holder="foo")
        self.assertEqual(address, messages.address("broker", holder="foo"))
        messages.key_path().unlink()
        self.assertIsNone(messages.target(address, holder="foo"))

    def test_the_key_is_private(self) -> None:
        messages.address("broker", holder="foo")
        self.assertEqual(0o600, messages.key_path().stat().st_mode & 0o777)

    def test_names_never_look_like_addresses(self) -> None:
        from bottle.bottles import NAME_PATTERN
        self.assertIsNone(NAME_PATTERN.fullmatch("broker~abc"))
        self.assertFalse(messages.is_address("app-1a2b3c4d-foo"))


if __name__ == "__main__":
    unittest.main()
