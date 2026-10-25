"""Self-managed groups (#174): the pure helpers E124 builds on — the master's prompt,
the task queue, and the measurements (the bound on live groups, orphans, wall time).

No kernel and no model: the ledger is fed samples by hand, the queue is built from
a fake exercise tree under `tmp_path`.
"""

from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import sys
from pathlib import Path

E2E = next(p for p in Path(__file__).resolve().parents if (p / "e2e" / ".e2e-root").exists()) / "e2e"
sys.path.insert(0, str(E2E))

import pytest  # noqa: E402
from helpers import groups  # noqa: E402

PROBE = E2E / "tests" / "agentic_advanced" / "E124_self_managed_groups" / "probe.py"


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


# --- the probe's pure functions (#174): max_turn_input, run_suite, worker_record ---


@pytest.fixture
def probe_env(monkeypatch):
    for var in ("ANTHROPIC_API_KEY", "ANTHROPIC_ENDPOINT", "FANTASTIC_AI_MODEL", "BACKEND"):
        monkeypatch.delenv(var, raising=False)
    for var in list(os.environ):
        if var.startswith("FANTASTIC_AI_MODEL_"):
            monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("BACKEND", "anthropic")
    monkeypatch.setenv("ANTHROPIC_ENDPOINT", "https://api.example.test/v1/messages")
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-ant-TEST-KEY-DO-NOT-LOG")
    monkeypatch.setenv("FANTASTIC_AI_MODEL", "test-model-default")
    monkeypatch.setenv("MAX_GROUPS", "2")
    monkeypatch.setenv("EXERCISES", "alpha,beta")
    monkeypatch.setenv("MASTER_INPUT_CHARS_MAX", "100000")
    return monkeypatch


def _probe():
    spec = importlib.util.spec_from_file_location("e124_groups_probe", PROBE)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _row(role, n):
    return {"role": role, "content": "x" * n}


def test_max_turn_input_no_assistant_row_is_zero(probe_env):
    p = _probe()
    assert p.max_turn_input("abcd", []) == 0
    assert p.max_turn_input("abcd", [_row("user", 5), _row("user", 9)]) == 0


def test_max_turn_input_counts_prompt_plus_conversation_before_reply(probe_env):
    p = _probe()
    assert p.max_turn_input("p" * 10, [_row("user", 5), _row("assistant", 7)]) == 15


def test_max_turn_input_picks_largest_turn(probe_env):
    p = _probe()
    conv = [_row("user", 5), _row("assistant", 3), _row("user", 20), _row("assistant", 1),
            _row("user", 2), _row("assistant", 1)]
    assert p.max_turn_input("p" * 10, conv) == 41


def test_max_turn_input_counts_earlier_replies_in_later_turns(probe_env):
    p = _probe()
    conv = [_row("user", 1), _row("assistant", 100), _row("user", 1), _row("assistant", 1)]
    assert p.max_turn_input("", conv) == 102


def test_max_turn_input_counts_non_assistant_roles_and_missing_content(probe_env):
    p = _probe()
    conv = [_row("tool", 6), {"role": "user", "content": None}, {"role": "user"}, _row("assistant", 1)]
    assert p.max_turn_input("p" * 4, conv) == 10


def test_run_suite_true_on_passing_exercise(probe_env, tmp_path):
    p = _probe()
    (tmp_path / "test_ok.py").write_text("def test_ok():\n    assert True\n")
    ok, line = p.run_suite(str(tmp_path))
    assert ok is True
    assert "passed" in line


def test_run_suite_false_on_failing_exercise(probe_env, tmp_path):
    p = _probe()
    (tmp_path / "test_bad.py").write_text("def test_bad():\n    assert False\n")
    ok, line = p.run_suite(str(tmp_path))
    assert ok is False
    assert "failed" in line


def test_run_suite_false_on_no_tests_collected(probe_env, tmp_path):
    p = _probe()
    ok, _ = p.run_suite(str(tmp_path))
    assert ok is False


def test_run_suite_timeout_scores_red(probe_env, tmp_path, monkeypatch):
    p = _probe()

    def boom(*a, **k):
        raise subprocess.TimeoutExpired(cmd="x", timeout=300)

    monkeypatch.setattr(p.subprocess, "run", boom)
    assert p.run_suite(str(tmp_path)) == (False, "suite timed out (300s)")


def test_run_suite_summary_line_is_last_nonblank_and_capped_at_80(probe_env, tmp_path, monkeypatch):
    p = _probe()
    done = subprocess.CompletedProcess(args=[], returncode=0, stdout="a\nb\n\n" + "z" * 200 + "\n\n", stderr="")
    monkeypatch.setattr(p.subprocess, "run", lambda *a, **k: done)
    assert p.run_suite(str(tmp_path)) == (True, "z" * 80)


def _worker_prompt(p):
    return groups.master_prompt(2, p.BK.model_for("worker"), "/q", worker_record=p.worker_record())


def test_worker_record_omits_model_the_master_sets_it(probe_env):
    probe_env.setenv("FANTASTIC_AI_MODEL_WORKER", "worker-x")
    p = _probe()
    rec = p.worker_record()
    assert "model" not in rec
    assert rec["context_window"] == p.CTX == 65536
    assert rec["handler_module"]
    assert rec["can_call"]["$parent"] == "*"
    assert p.creds.CREDS in rec["can_call"]


def test_worker_model_comes_from_worker_override(probe_env):
    probe_env.setenv("FANTASTIC_AI_MODEL_WORKER", "worker-x")
    probe_env.setenv("FANTASTIC_AI_MODEL_MASTER", "master-y")
    p = _probe()
    assert p.BK.model_for("worker") == "worker-x"
    text = _worker_prompt(p)
    assert "`model` = `worker-x`" in text
    assert "master-y" not in text


def test_worker_model_falls_back_to_default_model(probe_env):
    p = _probe()
    assert p.BK.model_for("worker") == "test-model-default"
    assert "`model` = `test-model-default`" in _worker_prompt(p)


def test_worker_override_for_another_role_does_not_leak(probe_env):
    probe_env.setenv("FANTASTIC_AI_MODEL_REVIEWER", "sonnet-y")
    p = _probe()
    assert p.BK.model_for("worker") == "test-model-default"
    assert "sonnet-y" not in _worker_prompt(p)
