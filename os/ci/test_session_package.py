"""Unit tests of the files `fantastic-session` ships for the system canvas (os/session/):
the user unit, the tmpfiles line, the polkit rule and the maintainer scripts
(stdlib unittest, plain python3).

    python3 -m unittest discover -s os/ci -p 'test_*.py'

What the .deb does with them is the `debs` install test's (build.yml); here: what
they say. The unit is read as the INI it is, the scripts as shell text and through
`sh -n`.
"""

import configparser
import json
import re
import shlex
import subprocess
import unittest
from pathlib import Path

SESSION = Path(__file__).resolve().parent.parent / "session"
UNIT = SESSION / "usr/lib/systemd/user/fantastic-system.service"
TMPFILES = SESSION / "usr/lib/tmpfiles.d/fantastic-session.conf"
POLKIT = SESSION / "usr/share/polkit-1/rules.d/50-fantastic-session.rules"
POSTINST = SESSION / "DEBIAN/postinst"
POSTRM = SESSION / "DEBIAN/postrm"
FOLDER = "/home/fantastic/system"


def unit() -> configparser.ConfigParser:
    """The unit file: keys may repeat (`strict=False`), `%t` and `$` are not interpolated."""
    cp = configparser.ConfigParser(strict=False, interpolation=None, delimiters=("=",))
    cp.optionxform = str  # systemd keys are case-sensitive
    cp.read_string(UNIT.read_text())
    return cp


def shell_branch(script: str, condition: str, holding: str = "") -> str:
    """The body of the first `if ...; then ... fi` whose first line contains `condition`
    and whose body contains `holding`."""
    lines = script.splitlines()
    for start, first in enumerate(lines):
        if not (first.startswith("if ") and condition in first):
            continue
        depth, body = 0, []
        for ln in lines[start:]:
            s = ln.strip()
            if s.startswith("if "):
                depth += 1
            elif s == "fi":
                depth -= 1
                if depth == 0:
                    break
            body.append(ln)
        if holding in "\n".join(body):
            return "\n".join(body)
    raise AssertionError(f"no `if` with {condition!r} holding {holding!r}")


class Unit(unittest.TestCase):
    def test_system_unit_is_type_notify_with_condition_user_and_execstart(self):
        u = unit()
        self.assertEqual(u["Service"]["Type"], "notify", "ExecStartPost waits for READY=1")
        self.assertEqual(u["Unit"]["ConditionUser"], "fantastic", "enabled for every manager, runs in the kiosk user's")
        self.assertEqual(u["Unit"]["StartLimitIntervalSec"], "0")
        self.assertEqual(u["Service"]["ExecStart"], "/usr/bin/fantastic canvas system")
        self.assertEqual(u["Service"]["WorkingDirectory"], FOLDER)
        self.assertEqual((u["Service"]["Restart"], u["Service"]["RestartSec"]), ("always", "2"))
        env = dict(kv.split("=", 1) for kv in shlex.split(u["Service"]["Environment"]))
        self.assertEqual(env, {"DBUS_SESSION_BUS_ADDRESS": "unix:path=%t/bus", "WAYLAND_DISPLAY": "wayland-0"})
        self.assertEqual(u["Install"]["WantedBy"], "default.target")
        # a user unit is not tied to a login session
        self.assertNotIn("User", u["Service"])
        self.assertNotIn("PAMName", u["Service"])

    def test_system_unit_has_register_home_folder_execstartpost(self):
        argv = shlex.split(unit()["Service"]["ExecStartPost"])
        self.assertEqual(argv[:2], ["/usr/bin/fantastic", "send"])
        self.assertEqual(len(argv), 3)
        self.assertEqual(
            json.loads(argv[2]),
            {
                "target": "user",
                "payload": {
                    "type": "forward",
                    "target": "user_store",
                    "payload": {"type": "register_home_folder", "value": True},
                },
            },
        )


class Tmpfiles(unittest.TestCase):
    def test_tmpfiles_has_the_system_folder_line_0700_fantastic(self):
        lines = [ln.split() for ln in TMPFILES.read_text().splitlines() if ln.strip() and not ln.startswith("#")]
        self.assertIn(["d", FOLDER, "0700", "fantastic", "fantastic", "-"], lines)
        # only directories are made, never removed (the age is `-`)
        self.assertTrue(all(ln[0] == "d" and ln[-1] == "-" for ln in lines), lines)


class Polkit(unittest.TestCase):
    def test_polkit_rule_grants_networkmanager_and_login1_power_actions_to_fantastic(self):
        rule = POLKIT.read_text()
        # only the kiosk user
        self.assertIn('if (subject.user !== "fantastic") return polkit.Result.NOT_HANDLED;', rule)
        # every NetworkManager action
        self.assertIn(
            'if (action.id.indexOf("org.freedesktop.NetworkManager.") === 0) return polkit.Result.YES;', rule
        )
        # logind's power actions, whatever the session's state
        cases = re.findall(r'case "(org\.freedesktop\.login1\.[a-z-]+)":', rule)
        self.assertEqual(
            sorted(cases),
            sorted(
                f"org.freedesktop.login1.{a}"
                for a in (
                    "power-off",
                    "reboot",
                    "suspend",
                    "hibernate",
                    "power-off-multiple-sessions",
                    "reboot-multiple-sessions",
                )
            ),
        )
        yes = rule.index("return polkit.Result.YES;", rule.index("switch (action.id)"))
        self.assertTrue(all(rule.index(f'case "{c}"') < yes for c in cases), "every case falls to the one YES")
        # and nothing else: the rest is left to the other rules
        self.assertTrue(rule.rstrip().endswith("return polkit.Result.NOT_HANDLED;\n});"), rule[-120:])


class Scripts(unittest.TestCase):
    def test_scripts_are_valid_shell(self):
        for script in (POSTINST, POSTRM):
            with self.subTest(script.name):
                subprocess.run(["sh", "-n", str(script)], check=True)

    def test_postinst_enables_the_user_unit_and_calls_tmpfiles(self):
        text = POSTINST.read_text()
        # in the branch that runs on configure and on the aborts, after the kiosk's own
        branch = shell_branch(text, 'configure ] || [ "$1" = abort-upgrade', holding="systemd-tmpfiles")
        self.assertIn("systemd-tmpfiles --create fantastic-session.conf", branch)
        self.assertIn("deb-systemd-helper --quiet --user was-enabled fantastic-system.service", branch)
        self.assertIn("deb-systemd-helper --user enable fantastic-system.service", branch)
        self.assertIn("deb-systemd-helper --user update-state fantastic-system.service", branch)
        # a failure of any of them is not the install's (|| true)
        for ln in branch.splitlines():
            if "systemd-tmpfiles --create" in ln or "--user enable" in ln or "--user update-state" in ln:
                self.assertIn("|| true", ln, ln)
        self.assertLess(text.index("fantastic-kiosk.service"), text.index("fantastic-system.service"))
        self.assertTrue(text.rstrip().endswith("exit 0"))

    def test_postrm_purge_removes_the_user_unit_link_and_keeps_the_folder(self):
        text = POSTRM.read_text()
        purge = shell_branch(text, '"$1" = purge')
        self.assertIn("deb-systemd-helper --user purge fantastic-system.service", purge)
        self.assertIn("|| true", next(ln for ln in purge.splitlines() if "--user purge" in ln))
        # the folder is the user's data: nothing in the script removes it
        self.assertNotIn(FOLDER, text.replace("# the user unit's link; the folder /home/fantastic/system is the user's data and stays", ""))
        self.assertNotRegex(text, r"\brm\b")
        self.assertNotIn("userdel", text)
        # and a plain remove does not touch the user unit's link either
        remove = shell_branch(text, '"$1" = remove ]')
        self.assertNotIn("fantastic-system", remove)


if __name__ == "__main__":
    unittest.main()
