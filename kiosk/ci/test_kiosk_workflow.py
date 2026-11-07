"""Static test of `.github/workflows/kiosk.yml` (stdlib unittest; the yml is read as text):
the kiosk's units and the system agents on a fake bus are two parallel jobs, each in `kiosk/`.

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
