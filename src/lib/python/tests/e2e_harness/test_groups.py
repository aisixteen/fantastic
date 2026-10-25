"""Self-managed groups (#174): the pure helpers E124 builds on — the master's prompt,
the task queue, and the measurements (the bound on live groups, orphans, wall time).

No kernel and no model: the ledger is fed samples by hand, the queue is built from
a fake exercise tree under `tmp_path`.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

E2E = next(p for p in Path(__file__).resolve().parents if (p / "e2e" / ".e2e-root").exists()) / "e2e"
sys.path.insert(0, str(E2E))

from helpers import groups  # noqa: E402


def test_master_prompt_states_max_groups_and_worker_model():
    text = groups.master_prompt(3, "worker-model-xyz", "/w/queue")
    assert "3" in text
    assert "worker-model-xyz" in text
    assert "/w/queue" in text
    assert "max_groups = 3" in text


def test_master_prompt_names_only_kernel_verbs():
    text = groups.master_prompt(2, "m", "/q")
    for verb in ("create_agent", "send", "delete_agent"):
        assert verb in text, verb
    assert not re.search(r"\bgroup_\w+", text), re.findall(r"\bgroup_\w+", text)


def test_master_prompt_length_is_bounded():
    one = groups.master_prompt(1, "m", "/q")
    three = groups.master_prompt(3, "m", "/q")
    assert len(three) - len(one) < 200


def test_group_of_parses_prefix():
    assert groups.group_of("g2_worker") == 2
    assert groups.group_of("g12_runner") == 12
    assert groups.group_of("notes") is None
    assert groups.group_of("gx_") is None
    assert groups.group_of("w3_worker", prefix="w") == 3


def test_ledger_max_live_counts_distinct_groups():
    led = groups.GroupLedger()
    led.observe(0.0, {"g1_a", "g1_b"})
    led.observe(1.0, {"g1_a", "g2_a"})
    assert led.max_live() == 2


def test_ledger_over_detects_breach():
    led = groups.GroupLedger()
    led.observe(0.0, {"g1_worker"})
    led.observe(1.0, {"g1_worker", "g2_worker", "g3_runner", "notes"})
    assert led.over(2) is True
    assert led.over(3) is False


def test_ledger_orphans_lists_leftover_group_agents():
    led = groups.GroupLedger()
    led.observe(0.0, {"g1_worker", "g2_runner", "notes"})
    assert led.orphans({"g2_runner", "notes"}) == ["g2_runner"]


def test_ledger_no_orphans_when_all_deleted():
    led = groups.GroupLedger()
    led.observe(0.0, {"g1_worker", "g1_runner"})
    led.observe(5.0, {"notes"})
    assert led.orphans({"notes", "ai", "kernel"}) == []
    assert led.max_live() == 1


def test_ledger_lifetimes_first_and_last_seen():
    led = groups.GroupLedger()
    led.observe(1.0, {"g1_worker"})
    led.observe(2.0, {"g1_worker", "g2_worker"})
    led.observe(4.0, {"g2_runner"})
    led.observe(6.0, set())
    assert led.lifetimes() == {1: (1.0, 2.0), 2: (2.0, 4.0)}
    assert groups.wall_seconds(led) == 5.0


def test_wall_seconds_of_an_empty_ledger_is_zero():
    assert groups.wall_seconds(groups.GroupLedger()) == 0.0


def test_compare_reports_speedup():
    out = groups.compare(300.0, 120.0)
    assert out == {"wall_1": 300.0, "wall_3": 120.0, "speedup": 2.5}
    assert groups.compare(10.0, 0.0)["speedup"] is None
    one, three = groups.GroupLedger(), groups.GroupLedger()
    for t in (0.0, 300.0):
        one.observe(t, set())
    for t in (0.0, 120.0):
        three.observe(t, set())
    assert groups.compare(one, three)["speedup"] == 2.5


def test_make_queue_copies_exercises_without_leaking_solutions(tmp_path):
    practice = tmp_path / "practice"
    ex = practice / "leap"
    (ex / ".meta").mkdir(parents=True)
    (ex / ".meta" / "example.py").write_text("SOLUTION")
    (ex / ".docs").mkdir()
    (ex / ".docs" / "instructions.md").write_text("Is it a leap year?")
    (ex / "leap.py").write_text("def leap_year(year):\n    pass\n")
    (ex / "leap_test.py").write_text("from leap import leap_year\n")
    work = tmp_path / "work"

    tasks = groups.make_queue(["leap"], work, practice=practice)

    dst = work / "queue" / "leap"
    assert (dst / "leap.py").exists()
    assert (dst / "leap_test.py").read_text() == "from leap import leap_year\n"
    assert not (dst / ".meta").exists()
    assert not (dst / ".docs").exists()
    assert not any("SOLUTION" in p.read_text() for p in dst.rglob("*") if p.is_file())
    assert len(tasks) == 1
    t = tasks[0]
    assert t["name"] == "leap"
    assert t["dir"] == str(dst)
    assert t["tests"] == ["leap_test.py"]
    assert t["stubs"] == ["leap.py"]
    assert "leap year" in t["instructions"]
