"""Static test of the units split in build.yml: cli, system agents (fake bus) and the
desktop app are three parallel jobs (stdlib unittest; the yml is read as text).

    python3 -m unittest discover -s os/ci -p 'test_ci_units_split.py'
"""

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
BUILD = (REPO / ".github" / "workflows" / "build.yml").read_text()


def job(name):
    m = re.search(rf"^  {re.escape(name)}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|^  #|\Z)", BUILD, re.S | re.M)
    assert m, f"job {name} missing"
    return m.group(1)


class UnitsSplit(unittest.TestCase):
    def test_three_jobs_under_units(self):
        for name in ("cli", "system-agents", "desktop-app"):
            self.assertIn("if: inputs.units\n", job(name), name)

    def test_desktop_app_holds_the_app_tests(self):
        j = job("desktop-app")
        self.assertIn("working-directory: src/app", j)
        self.assertIn("cargo test --locked --no-fail-fast", j)

    def test_fake_bus_holds_ignored(self):
        j = job("system-agents")
        self.assertIn("-- --ignored", j)
        self.assertIn("runs-on: ubuntu-latest", j)

    def test_cli_holds_neither(self):
        j = job("cli")
        self.assertNotIn("--ignored", j)
        self.assertNotIn("src/app", j)
        self.assertNotIn("dbusmock", j)

    def test_every_rust_cache_saves_on_main(self):
        saves = re.findall(r"save-if: (.*)", BUILD)
        self.assertTrue(saves)
        for s in saves:
            self.assertIn("refs/heads/main", s)

    def test_rust_cache_keys_differ_per_job(self):
        keys = [re.search(r"key: (.*)", job(n)).group(1) for n in ("cli", "system-agents", "desktop-app")]
        self.assertEqual(len(set(keys)), 3)


if __name__ == "__main__":
    unittest.main()
