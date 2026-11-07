"""Static test of `.github/workflows/kiosk.yml` (stdlib unittest; the yml is read as text):
the kiosk's units and the system agents on a fake bus are two parallel jobs, each in `kiosk/`; the session package and the image are two more.

    python3 -m unittest discover -s kiosk/ci -p 'test_kiosk_workflow.py'
"""

import re
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
WORKFLOW = (REPO / ".github" / "workflows" / "kiosk.yml").read_text()


def job(name):
    m = re.search(rf"^  {re.escape(name)}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|^  #|\Z)", WORKFLOW, re.S | re.M)
    assert m, f"job {name} missing"
    return m.group(1)


class KioskWorkflow(unittest.TestCase):
    def test_both_jobs_work_in_the_kiosk_folder(self):
        for name in ("units", "system-agents"):
            self.assertIn("working-directory: kiosk", job(name), name)

    def test_fake_bus_holds_ignored(self):
        j = job("system-agents")
        self.assertIn("-- --ignored", j)
        self.assertIn("runs-on: ubuntu-latest", j)
        self.assertIn("python3-dbusmock", j)

    def test_units_hold_neither_the_bus_nor_ignored(self):
        j = job("units")
        self.assertNotIn("--ignored", j)
        self.assertNotIn("dbusmock", j)

    def test_it_runs_only_for_the_kiosk(self):
        self.assertEqual(WORKFLOW.count('"kiosk/**"'), 2)


if __name__ == "__main__":
    unittest.main()


class KioskImageJobs(unittest.TestCase):
    def test_the_session_package_and_the_image_are_jobs_of_the_kiosk(self):
        self.assertIn("kiosk/os/ci", job("session-package"))
        self.assertIn("shellcheck", job("session-package"))
        image = job("image")
        for step in ("kiosk/os/build/debs.sh", "kiosk/os/build/os.sh", "kiosk/os/ci/boot_check.py", "kiosk/os/ci/e2e_system.py"):
            self.assertIn(step, image, step)

    def test_the_image_pulls_nothing_from_the_base_workflows(self):
        self.assertNotIn("scripts/build/deb.sh", job("image"), "the base's .deb comes from its Release")
