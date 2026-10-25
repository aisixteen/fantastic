"""SELF-MANAGED GROUPS (#174) — the pure half of E124: the master's prompt, the task
queue, and the measurements. No kernel, no model, no network.

A master AI runs up to `max_groups` worker groups at once, from its prompt alone. The
substrate has no group code and needs none: a group is the agents the master creates
under one id prefix (`g<k>_worker`, `g<k>_editor`, `g<k>_runner`) with the kernel
verbs every AI already has (`create_agent`, `send`, `delete_agent`). So a group is a
NAMING convention, and this module only reads names: `group_of` parses the prefix,
`GroupLedger` turns samples of live agent ids into the numbers the test is scored on
(the most groups live at once, the agents left behind, how long each group lived).
"""

from __future__ import annotations

import json
import re
import shutil
from pathlib import Path

#: Folders that leak the reference solution or are pure docs — never copied into a
#: workdir. The same set as `tests/coding_exercism/gym.py` (`EXCLUDE`); copied, not
#: imported, because importing the gym resolves a backend at import time.
EXCLUDE = {".meta", ".docs", ".approaches", ".git", "node_modules", "build", "target"}

#: Where the gym's clone of the polyglot benchmark lives (`gym.BENCH`).
PRACTICE = Path(__file__).resolve().parents[2] / "tmp" / "repos" / "polyglot-benchmark" / "python" / "exercises" / "practice"


def master_prompt(
    max_groups: int,
    worker_model: str,
    queue_dir: str,
    group_prefix: str = "g",
    *,
    worker_record: dict | None = None,
    test_command: list[str] | None = None,
) -> str:
    """The master's `pre_system_prompt`. It states the bound, the worker model and the
    one-group-at-a-time recipe, and names only kernel verbs. The task list is NOT in
    it (the master reads the queue), so its length does not grow with the queue and
    barely with `max_groups`.

    `worker_record` is the record fields every worker gets verbatim (the plug's
    handler, endpoint, key reference, memory store); `test_command` the argv the
    group's runner may start."""
    p = group_prefix
    cmd = test_command or ["python", "-m", "pytest", "-q"]
    start = json.dumps({"type": "start", "command": cmd[0], "args": cmd[1:]})
    fields = json.dumps(worker_record or {}, sort_keys=True)
    return (
        "You are the MASTER. You finish a queue of coding tasks by running worker "
        "groups. You never write code yourself.\n\n"
        f"THE BOUND: max_groups = {max_groups}. Never have more than {max_groups} "
        "groups alive at once. A group is alive from its first `create_agent` until "
        "all its agents are deleted.\n\n"
        f"THE QUEUE: one folder per task under `{queue_dir}`. Read it with the file "
        "agent `queue` (verb `list`, then `read` the task's README.task.md). Each "
        "folder holds a stub and its test file.\n\n"
        f"A GROUP k (k = 1, 2, 3, … one new k per task, never reused) is three agents "
        f"you create with `create_agent` to `kernel`, `parent_id` = your own id:\n"
        f"  `{p}<k>_editor`: handler_module `file_bridge.tools`, root "
        f"`{queue_dir}/<task>`, allow [\".\"], callable_by {{{p}<k>_worker: *}}.\n"
        f"  `{p}<k>_runner`: handler_module `exec_bridge.tools`, cwd "
        f"`{queue_dir}/<task>`, inherit_env [\"PATH\", \"HOME\"], allowed "
        f"[{json.dumps(' '.join(cmd) + '.*')}], callable_by {{"
        f"{p}<k>_worker: *, $parent: *}}.\n"
        f"  `{p}<k>_worker`: an AI. Copy these fields verbatim: {fields}; set "
        f"`model` = `{worker_model}`; add `{p}<k>_editor: *` and `{p}<k>_runner: *` to "
        "its can_call, and callable_by {$parent: *}.\n"
        f"The suite runs with this payload to the runner: {start} (then poll "
        "`status` with its job_id until it is not running, `tail` for the output).\n\n"
        "THE RECIPE, repeat until the queue is empty:\n"
        f"  1. While live groups < {max_groups} and tasks are left: create the next "
        "group (one batch of three `create_agent`), then `send` its worker the task "
        "(the task's instructions, its stub and test file names, the editor and "
        "runner ids, the start payload, and: never edit the test file; when the "
        "suite exits 0, reply to me `DONE <task>`), with reply_to null.\n"
        "  2. Call `done` and wait. A worker's `DONE <task>` wakes you.\n"
        "  3. Check it: start the suite on that group's runner yourself and poll it. "
        "Green: the task is finished. Red: `send` the worker the failure once more.\n"
        f"  4. Delete the finished group's three agents (`delete_agent` to `kernel`, "
        "one batch), then go to 1.\n"
        "When every task is finished and no group is alive, say ALL DONE and call "
        "`done`."
    )


def make_queue(names: list[str], workroot: str | Path, *, practice: str | Path | None = None) -> list[dict]:
    """Copy the named Exercism python exercises into `workroot/queue/<name>/`, minus
    the solution and doc folders (`EXCLUDE`), and return the task list: name, dir,
    stub and test file names, and the instructions (read from the source's
    `.docs`, never copied). The instructions also land as `README.task.md` in the
    task's folder, so the master can read a task without it being in its prompt."""
    src_root = Path(practice) if practice is not None else PRACTICE
    queue = Path(workroot) / "queue"
    queue.mkdir(parents=True, exist_ok=True)
    tasks: list[dict] = []
    for name in names:
        ex = src_root / name
        if not ex.is_dir():
            raise FileNotFoundError(f"no exercise {name!r} under {src_root}")
        dst = queue / name
        dst.mkdir(parents=True, exist_ok=True)
        for item in ex.iterdir():
            if item.name in EXCLUDE:
                continue
            if item.is_dir():
                shutil.copytree(item, dst / item.name, ignore=shutil.ignore_patterns(*EXCLUDE))
            else:
                shutil.copy2(item, dst / item.name)
        doc = ex / ".docs" / "instructions.md"
        instructions = doc.read_text() if doc.exists() else "(no instructions)"
        (dst / "README.task.md").write_text(instructions)
        files = sorted(p.name for p in dst.glob("*.py") if p.is_file())
        tests = [f for f in files if f.endswith("_test.py")]
        tasks.append(
            {
                "name": name,
                "dir": str(dst),
                "stubs": [f for f in files if f not in tests],
                "tests": tests,
                "instructions": instructions,
            }
        )
    return tasks


def group_of(agent_id: str, prefix: str = "g") -> int | None:
    """The group index of `<prefix><k>_<role>`, or None for an id outside any group."""
    m = re.fullmatch(rf"{re.escape(prefix)}(\d+)_\w+", agent_id)
    return int(m.group(1)) if m else None


class GroupLedger:
    """Samples of live agent ids over time, read as groups."""

    def __init__(self, prefix: str = "g") -> None:
        self.prefix = prefix
        self.samples: list[tuple[float, frozenset[str]]] = []

    def observe(self, t: float, live_ids) -> None:
        """One sample: the ids live at time `t`."""
        self.samples.append((t, frozenset(live_ids)))

    def _groups(self, ids) -> set[int]:
        return {g for g in (group_of(i, self.prefix) for i in ids) if g is not None}

    def max_live(self) -> int:
        """The most DISTINCT groups with any agent live in one sample."""
        return max((len(self._groups(ids)) for _, ids in self.samples), default=0)

    def over(self, n: int) -> bool:
        """Did any sample see more than `n` groups live at once?"""
        return self.max_live() > n

    def orphans(self, final_live_ids) -> list[str]:
        """The group agents still live at the end (ids outside any group ignored)."""
        return sorted(i for i in final_live_ids if group_of(i, self.prefix) is not None)

    def lifetimes(self) -> dict[int, tuple[float, float]]:
        """group -> (first sample it was seen live in, last sample it was seen in)."""
        out: dict[int, tuple[float, float]] = {}
        for t, ids in self.samples:
            for g in self._groups(ids):
                first, _ = out.get(g, (t, t))
                out[g] = (first, t)
        return dict(sorted(out.items()))


def wall_seconds(ledger: GroupLedger) -> float:
    """From the first sample to the last: the run's wall time as the ledger saw it."""
    if not ledger.samples:
        return 0.0
    return ledger.samples[-1][0] - ledger.samples[0][0]


def compare(run1: float | GroupLedger, run3: float | GroupLedger) -> dict:
    """The 1-vs-3 wall time: the wall of the MAX_GROUPS=1 run, of the MAX_GROUPS=3
    run (seconds, or the run's ledger), and the speedup (None when the second wall
    is zero)."""
    run1 = wall_seconds(run1) if isinstance(run1, GroupLedger) else float(run1)
    run3 = wall_seconds(run3) if isinstance(run3, GroupLedger) else float(run3)
    return {"wall_1": run1, "wall_3": run3, "speedup": (run1 / run3) if run3 else None}
