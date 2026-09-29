import os
import subprocess
import unittest
from contextlib import contextmanager
from pathlib import Path
from unittest import mock

from bottle import auth, features, runtime
from bottle.errors import BottleError

# What `claude setup-token` printed, as recorded with `script` (token replaced).
RECORDED = (
    "^M^[[1C^[[9A^[[K^M^[[1C^[[1B^[[32m✓ Long-lived authentication token created successfully!^M^[[1B^[[39m^[[K"
    "^M^[[1B Your OAuth token (valid for 1 year):^[[K^M^[[1C^[[2B^[[93msk-ant-oat01-EXAMPLE_token-123^M^[[1C^[[2B"
    "^[[37mStore this token securely. You won't be able to see it again.^M^[[1C^[[1B^[[39m^[[K^M^M"
).replace("^[", "\x1b").replace("^M", "\r")


class CaptureTest(unittest.TestCase):
    pattern = auth.features.load("claude").credentials[0].login.capture

    def test_the_recorded_claude_output(self) -> None:
        self.assertEqual(auth.capture(self.pattern, RECORDED), "sk-ant-oat01-EXAMPLE_token-123")

    def test_a_token_wrapped_over_lines_by_a_narrow_terminal(self) -> None:
        wrapped = RECORDED.replace(
            "sk-ant-oat01-EXAMPLE_token-123",
            "sk-ant-oat01-EXAMPLE\r\x1b[1C\x1b[1B\x1b[93m_token-123",
        )
        self.assertEqual(auth.capture(self.pattern, wrapped), "sk-ant-oat01-EXAMPLE_token-123")

    def test_no_match(self) -> None:
        self.assertIsNone(auth.capture(self.pattern, "Login cancelled\r\n"))

    def test_run_captured_records_output(self) -> None:
        output = auth.run_captured(["sh", "-c", "printf 'Your OAuth token:\\r\\n\\033[93mabc-123\\r\\nStore this token\\r\\n'"])
        self.assertEqual(auth.capture(self.pattern, output), "abc-123")


class FakeKeychain:
    """Stands in for `security`, recording every command line it's given."""

    def __init__(self) -> None:
        self.items: dict[tuple[str, str], str] = {}
        self.argvs: list[list[str]] = []

    def run(self, argv, input=None, capture_output=False, text=False):
        self.argvs.append(argv)
        if argv[:2] == ["security", "-i"]:
            words = input.split('"')
            service, account, value = words[1], words[3], words[5]
            self.items[(service, account)] = value
            return subprocess.CompletedProcess(argv, 0, "", "")
        service, account = argv[argv.index("-s") + 1], argv[argv.index("-a") + 1]
        if argv[1] == "find-generic-password":
            value = self.items.get((service, account))
            return subprocess.CompletedProcess(argv, 0 if value else 44, (value or "") + "\n", "")
        if argv[1] == "delete-generic-password":
            return subprocess.CompletedProcess(argv, 0 if self.items.pop((service, account), None) else 44, "", "")
        raise AssertionError(argv)


class StoreTest(unittest.TestCase):
    def setUp(self) -> None:
        self.keychain = FakeKeychain()
        for patcher in (
            mock.patch.object(auth.subprocess, "run", side_effect=self.keychain.run),
            mock.patch.dict(os.environ, {"BOTTLE_HOME": str(Path.home() / ".bottle")}),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_put_get_delete(self) -> None:
        self.assertIsNone(auth.get("claude"))
        auth.put("claude", "tok-123")
        self.assertEqual(auth.get("claude"), "tok-123")
        self.assertTrue(auth.delete("claude"))
        self.assertFalse(auth.delete("claude"))
        self.assertIsNone(auth.get("claude"))

    def test_values_never_go_on_a_command_line(self) -> None:
        auth.put("claude", "tok-secret")
        self.assertFalse(any("tok-secret" in arg for argv in self.keychain.argvs for arg in argv))

    def test_rejects_values_it_cant_store_safely(self) -> None:
        for value in ("", 'a"b', "a\\b", "a\nb", "a\tb"):
            with self.subTest(value), self.assertRaises(BottleError):
                auth.put("claude", value)

    def test_default_home_uses_the_plain_service(self) -> None:
        self.assertEqual(auth._service(), "bottle")

    def test_other_homes_get_their_own_service(self) -> None:
        with mock.patch.dict(os.environ, {"BOTTLE_HOME": "/tmp/elsewhere"}):
            self.assertRegex(auth._service(), r"^bottle:[0-9a-f]{12}$")


class DeclaredTest(unittest.TestCase):
    def test_claude_declares_its_token(self) -> None:
        d = auth.get_declared("claude")
        self.assertEqual((d.feature, d.credential.env), ("claude", "CLAUDE_CODE_OAUTH_TOKEN"))

    def test_unknown(self) -> None:
        with self.assertRaisesRegex(BottleError, "no feature declares a credential named 'nope' \\(known: claude, teamcity\\)"):
            auth.get_declared("nope")

    def test_env_for_only_the_bottles_features(self) -> None:
        with mock.patch.object(auth, "get", return_value="tok"):
            self.assertEqual(auth.env_for(["claude", "tools"]), {"CLAUDE_CODE_OAUTH_TOKEN": "tok"})
            self.assertEqual(auth.env_for(["tools"]), {})


class InjectedCredentialTest(unittest.TestCase):
    """A credential the egress proxy holds: nothing about it enters the bottle."""

    specs = ["teamcity:server=https://ci.corp.example.com", "tools"]

    def test_nothing_is_delivered(self) -> None:
        with mock.patch.object(auth, "get", return_value="tc-token"):
            self.assertEqual(auth.env_for(self.specs), {})

    def test_the_proxy_gets_the_real_one(self) -> None:
        with mock.patch.object(auth, "get", return_value="tc-token"):
            [injection] = auth.injections_for(self.specs)
        self.assertEqual(injection.host, "ci.corp.example.com")
        self.assertEqual((injection.header, injection.value), ("Authorization", "Bearer tc-token"))
        self.assertEqual(injection.port, 443)

    def test_nothing_is_injected_until_it_is_logged_in(self) -> None:
        with mock.patch.object(auth, "get", return_value=None):
            self.assertEqual(auth.injections_for(self.specs), [])

    def test_a_delivered_credential_isnt_injected(self) -> None:
        with mock.patch.object(auth, "get", return_value="tok"):
            self.assertEqual(auth.injections_for(["claude"]), [])
            self.assertEqual(auth.env_for(["claude"]), {"CLAUDE_CODE_OAUTH_TOKEN": "tok"})

    def test_one_host_belongs_to_one_credential(self) -> None:
        claimed = features.Credential(
            "other", "Another token",
            inject=features.Inject(hosts=("ci.corp.example.com",), value="Bearer ${credential}"),
        )
        with mock.patch.object(auth, "get", return_value="tok"), \
                mock.patch.object(auth, "credentials_for", return_value=[
                    auth.credentials_for(self.specs)[0], claimed]), \
                self.assertRaisesRegex(BottleError, "both claim ci.corp.example.com"):
            auth.injections_for(self.specs)


class DestinationTest(unittest.TestCase):
    def test_a_host_is_reached_over_https(self) -> None:
        self.assertEqual(auth.destination("ci.example.com"), ("ci.example.com", 443))
        self.assertEqual(auth.destination("*.example.com"), ("*.example.com", 443))

    def test_a_host_may_name_its_port(self) -> None:
        self.assertEqual(auth.destination("ci.example.com:8111"), ("ci.example.com", 8111))

    def test_a_host_is_only_a_host(self) -> None:
        for host in ("ci.example.com:https", "ci.example.com:0", "ci.example.com:99999"):
            with self.subTest(host), self.assertRaisesRegex(BottleError, "isn't a host for a credential"):
                auth.destination(host)


class LoginTest(unittest.TestCase):
    def setUp(self) -> None:
        self.stored: dict[str, str] = {}
        self.ran: list[list[str]] = []

        @contextmanager
        def throwaway(features):
            self.features = features
            yield lambda argv: ["exec-in-throwaway", *argv]

        for patcher in (
            mock.patch.object(auth, "put", side_effect=self.stored.__setitem__),
            mock.patch("bottle.bottles.throwaway", throwaway),
            mock.patch.object(auth.sys.stdin, "isatty", return_value=True),
            mock.patch("sys.stderr"),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def run_login(self, output: str, pasted: str = "") -> None:
        def run_captured(argv):
            self.ran.append(argv)
            return output

        with mock.patch.object(auth, "run_captured", side_effect=run_captured), \
                mock.patch.object(auth.getpass, "getpass", return_value=pasted) as prompt:
            auth.login("claude")
        self.prompted = prompt.called

    def test_runs_the_declared_command_in_a_throwaway_bottle_and_captures(self) -> None:
        self.run_login(RECORDED)
        self.assertEqual(self.features, ["claude"])
        self.assertEqual(self.ran, [["exec-in-throwaway", "claude", "setup-token"]])
        self.assertEqual(self.stored, {"claude": "sk-ant-oat01-EXAMPLE_token-123"})
        self.assertFalse(self.prompted)

    def test_falls_back_to_pasting(self) -> None:
        self.run_login("something unexpected", pasted="  pasted-token\n")
        self.assertTrue(self.prompted)
        self.assertEqual(self.stored, {"claude": "pasted-token"})

    def test_needs_a_terminal(self) -> None:
        with mock.patch.object(auth.sys.stdin, "isatty", return_value=False), \
                self.assertRaisesRegex(BottleError, "use `bottle auth set claude`"):
            auth.login("claude")


class EnsureLoggedInTest(unittest.TestCase):
    def setUp(self) -> None:
        self.logged_in: list[str] = []
        for patcher in (
            mock.patch.object(auth, "login", side_effect=self.logged_in.append),
            mock.patch("sys.stderr"),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_logs_in_when_missing_at_a_terminal(self) -> None:
        with mock.patch.object(auth, "get", return_value=None), \
                mock.patch.object(auth.sys.stdin, "isatty", return_value=True):
            auth.ensure_logged_in(["claude", "tools"])
        self.assertEqual(self.logged_in, ["claude"])

    def test_nothing_to_do_when_set(self) -> None:
        with mock.patch.object(auth, "get", return_value="tok"):
            auth.ensure_logged_in(["claude"])
        self.assertEqual(self.logged_in, [])

    def test_without_a_terminal_only_says_how(self) -> None:
        with mock.patch.object(auth, "get", return_value=None), \
                mock.patch.object(auth.sys.stdin, "isatty", return_value=False), \
                mock.patch("sys.stderr") as err:
            auth.ensure_logged_in(["claude"])
        self.assertEqual(self.logged_in, [])
        self.assertIn("bottle auth login claude", "".join(c.args[0] for c in err.write.call_args_list))

    def test_features_without_credentials(self) -> None:
        auth.ensure_logged_in(["tools"])
        self.assertEqual(self.logged_in, [])


class ExecEnvTest(unittest.TestCase):
    def test_env_is_passed_by_name_only(self) -> None:
        with mock.patch.object(runtime.os, "execvp") as execvp, mock.patch.dict(os.environ, {}):
            runtime.container_exec_interactive("c", ["bash"], env={"TOKEN": "tok-secret"})
            self.assertEqual(os.environ["TOKEN"], "tok-secret")
        argv = execvp.call_args.args[1]
        self.assertIn("--env", argv)
        self.assertIn("TOKEN", argv)
        self.assertFalse(any("tok-secret" in arg for arg in argv))


if __name__ == "__main__":
    unittest.main()
