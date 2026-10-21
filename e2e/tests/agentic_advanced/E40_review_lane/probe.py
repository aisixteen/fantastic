"""LIVE TEST — REVIEW LANE: a reviewer AI checks a coder AI's work.

The harness builds the fixture and the two-agent team, kicks both off, and the pair
works unattended: the coder implements `ledger.py` from `TASK.md`, asks the reviewer
for a REVIEW, fixes what the reviewer reports, and asks again, until the reviewer
says PASS. The reviewer reads through a READ-ONLY file bridge and runs its own
checks through its own runner; it never edits.

The test owns its oracle. The visible tests (written to the workdir) cover only the
happy path. The HIDDEN oracle is never written there: the probe holds it and runs
it against the `ledger.py` that existed at each REVIEW and again at each verdict —
so a PASS on broken work is caught, not just a broken end state, and a file that
moved while a review was open (the reviewer's runner allows `python3` and `git`, so
"never edits" is a rule it could break) is reported as its own failure.

`python3 probe.py --selftest` checks the oracle and the pairing logic without a
model: a reference solution passes the oracle, the stub fails it, and a naive
solution passes the visible tests but fails the oracle.

An AI agent has no standing instructions on its record (a caller's `system_prompt`
REPLACES the whole default prompt), so each role rides in the KICKOFF message.
"""

from __future__ import annotations

import ast
import asyncio
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from backends import backend as _resolve_backend  # noqa: E402
from helpers import creds, memory  # noqa: E402
from helpers import verdict  # noqa: E402
from helpers.ai import IDLE_END, QuietWatch  # noqa: E402
from helpers.kernel import home, open_kernel  # noqa: E402

CTX = int(os.environ.get("CTX", "65536"))
UNATTENDED = float(os.environ.get("UNATTENDED", "900"))
CAP = int(os.environ.get("REVIEW_ROUNDS", "4"))
POLL = 2.0
W = 78

WORKDIR = "ledger_work"  # relative to the kernel's running dir
CODER, REVIEWER = "coder", "reviewer"
CODER_FB, CODER_RUN = "coder_fb", "coder_run"
REVIEWER_FB, REVIEWER_RUN = "reviewer_fb", "reviewer_run"
IDS = [CODER, REVIEWER, CODER_FB, CODER_RUN, REVIEWER_FB, REVIEWER_RUN]
MARK = "[E40 KICKOFF]"
# the runners may start python3 (the tests and one-off `-c` checks) and git
ALLOWED = [r"(?s)^python3?(\s.*)?$", r"(?s)^git(\s.*)?$"]

DONE_RE = re.compile("<" + r'call name="done"')

# ── the fixture ────────────────────────────────────────────────────────────────
TASK_MD = """# ledger.py — sum amounts per account

`ledger.py` holds a stub `parse(lines) -> dict`. Implement it.

`lines` is any iterable of strings (a trailing newline on a line is tolerated).
A data line has exactly three whitespace-separated fields:

    DATE ACCOUNT AMOUNT        e.g.  2026-10-01 rent -1200.00

* DATE has the shape YYYY-MM-DD (four digits, dash, two digits, dash, two
  digits). Its shape is checked; calendar validity is not.
* ACCOUNT is one token without whitespace.
* AMOUNT is an optional sign (`+` or `-`), one or more digits, and optionally a
  `.` followed by one or two digits. Nothing else is an amount: `1.234`, `.5`,
  `5.`, `--5`, `1e3` and `1,000.00` are malformed.

## Requirements

1. Ignore blank lines (empty or whitespace only) and comment lines (first
   non-space character is `#`).
2. Amounts may be negative or carry a leading `+` (`+10.50` is 10.50).
3. A malformed line raises `ValueError`. The message contains `line N` where N is
   the 1-based index of that line in the input, counting blank and comment
   lines too (a bad third input line gives a message containing `line 3`).
4. Use `decimal.Decimal`, never float. Return a dict mapping each account to its
   total as a `Decimal` with exactly two decimal places: `0.10 + 0.20` is
   `Decimal("0.30")` and `str()` of a total is `"0.30"`; a lone `5` is `5.00`.
5. An account whose lines sum to zero is still in the result (`0.00`). No input
   lines gives `{}`.

Run the visible tests with `python3 -m unittest -v test_ledger`. They cover only
the happy path; the requirements above are the whole contract.
"""

STUB = '''def parse(lines):
    raise NotImplementedError("implement parse() as described in TASK.md")
'''

VISIBLE_TESTS = '''import unittest
from decimal import Decimal

from ledger import parse


class VisibleTests(unittest.TestCase):
    def test_sums_per_account(self):
        lines = [
            "2026-10-01 rent -1200.00",
            "2026-10-02 salary 3000.00",
            "2026-10-03 rent -50.00",
        ]
        self.assertEqual(
            parse(lines),
            {"rent": Decimal("-1250.00"), "salary": Decimal("3000.00")},
        )

    def test_blank_lines_and_comments_are_ignored(self):
        lines = [
            "# october",
            "",
            "2026-10-01 food 12.50",
            "",
            "# a note",
            "2026-10-02 food 7.25",
        ]
        self.assertEqual(parse(lines), {"food": Decimal("19.75")})

    def test_negative_amount(self):
        self.assertEqual(
            parse(["2026-10-01 refund -5.00"]), {"refund": Decimal("-5.00")}
        )


if __name__ == "__main__":
    unittest.main()
'''

# THE HIDDEN ORACLE — never written to the workdir.
ORACLE = '''import unittest
from decimal import Decimal

from ledger import parse


def bad(tc, lines, n):
    with tc.assertRaises(ValueError) as cm:
        parse(lines)
    tc.assertIn("line %d" % n, str(cm.exception))


class Oracle(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(parse([]), {})

    def test_per_account_sum(self):
        r = parse(["2026-10-01 rent -1200.00", "2026-10-02 pay 3000.00",
                   "2026-10-03 rent -50.00"])
        self.assertEqual(r, {"rent": Decimal("-1250.00"), "pay": Decimal("3000.00")})

    def test_plus_sign(self):
        r = parse(["2026-10-01 pay +10.50", "2026-10-02 pay +0.25"])
        self.assertEqual(r, {"pay": Decimal("10.75")})

    def test_blank_and_comments(self):
        r = parse(["", "   ", "# c", "   # indented", "2026-10-01 a 1.00", "\\t"])
        self.assertEqual(r, {"a": Decimal("1.00")})

    def test_decimal_exact(self):
        r = parse(["2026-10-01 a 0.10", "2026-10-02 a 0.20"])
        self.assertIsInstance(r["a"], Decimal)
        self.assertEqual(r["a"], Decimal("0.30"))
        self.assertEqual(str(r["a"]), "0.30")

    def test_ten_tenths(self):
        r = parse(["2026-10-01 a 0.10"] * 10)
        self.assertEqual(str(r["a"]), "1.00")

    def test_two_places(self):
        r = parse(["2026-10-01 a 5", "2026-10-02 b 2.5", "2026-10-03 c -0.1"])
        self.assertEqual({k: str(v) for k, v in r.items()},
                         {"a": "5.00", "b": "2.50", "c": "-0.10"})

    def test_values_are_decimal(self):
        r = parse(["2026-10-01 a 1.5", "2026-10-02 b 2"])
        for v in r.values():
            self.assertIsInstance(v, Decimal)

    def test_trailing_newline(self):
        self.assertEqual(parse(["2026-10-01 a 1.00\\n"]), {"a": Decimal("1.00")})

    def test_zero_sum_account_kept(self):
        r = parse(["2026-10-01 a 1.00", "2026-10-02 a -1.00"])
        self.assertEqual(str(r["a"]), "0.00")

    def test_any_iterable(self):
        r = parse(iter(["2026-10-01 a 1.00", "2026-10-02 a 2.00"]))
        self.assertEqual(r, {"a": Decimal("3.00")})

    def test_malformed_missing_amount(self):
        bad(self, ["2026-10-01 rent"], 1)

    def test_malformed_amount_after_comment_and_blank(self):
        bad(self, ["# c", "", "2026-10-01 rent abc"], 3)

    def test_malformed_three_decimals(self):
        bad(self, ["2026-10-01 rent 1.234"], 1)

    def test_malformed_date(self):
        bad(self, ["2026-10-01 a 1.00", "2026/10/02 a 1.00"], 2)

    def test_malformed_extra_field(self):
        bad(self, ["2026-10-01 a 1.00 extra"], 1)

    def test_malformed_after_blanks(self):
        bad(self, ["", "", "", "", "2026-10-01 a 1.0.0"], 5)

    def test_malformed_double_sign(self):
        bad(self, ["2026-10-01 a --5"], 1)

    def test_malformed_exponent(self):
        bad(self, ["2026-10-01 a 1e3"], 1)

    def test_malformed_thousands_separator(self):
        bad(self, ["2026-10-01 a 1,000.00"], 1)

    def test_malformed_no_leading_digit(self):
        bad(self, ["2026-10-01 a .50"], 1)

    def test_malformed_trailing_dot(self):
        bad(self, ["2026-10-01 a 5."], 1)


if __name__ == "__main__":
    unittest.main()
'''

# A CORRECT reference solution (never written to the workdir) — the self-test only.
REFERENCE = '''import re
from decimal import Decimal

_LINE = re.compile(r"^(\\d{4}-\\d{2}-\\d{2})\\s+(\\S+)\\s+([+-]?\\d+(?:\\.\\d{1,2})?)$")


def parse(lines):
    totals = {}
    for n, raw in enumerate(lines, 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        m = _LINE.match(line)
        if m is None:
            raise ValueError("line %d: malformed ledger line: %r" % (n, line))
        account, amount = m.group(2), Decimal(m.group(3))
        totals[account] = totals.get(account, Decimal("0")) + amount
    return {k: v.quantize(Decimal("0.01")) for k, v in totals.items()}
'''

# A NAIVE solution: happy path only. Passes the visible tests, fails the oracle.
NAIVE = '''from decimal import Decimal


def parse(lines):
    out = {}
    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        _, account, amount = line.split()
        out[account] = out.get(account, Decimal("0")) + Decimal(amount)
    return {k: v.quantize(Decimal("0.01")) for k, v in out.items()}
'''


def _run_suite(ledger_text: str, test_file: str, test_src: str, module: str) -> tuple[bool, list[str]]:
    """Run a unittest module against `ledger_text` in a scratch dir."""
    with tempfile.TemporaryDirectory() as d:
        (Path(d) / "ledger.py").write_text(ledger_text)
        (Path(d) / test_file).write_text(test_src)
        try:
            p = subprocess.run(
                [sys.executable, "-m", "unittest", "-q", module],
                cwd=d, capture_output=True, text=True, timeout=60,
            )
        except subprocess.TimeoutExpired:
            return False, ["timeout (60s)"]
    failed = [ln.split(" (")[0] for ln in p.stderr.splitlines() if ln.startswith(("FAIL:", "ERROR:"))]
    return p.returncode == 0, failed


def run_oracle(ledger_text: str) -> tuple[bool, list[str]]:
    """The hidden oracle on a given ledger.py text: (passed, failing test names)."""
    return _run_suite(ledger_text, "oracle_test.py", ORACLE, "oracle_test")


def run_visible(ledger_text: str) -> tuple[bool, list[str]]:
    return _run_suite(ledger_text, "test_ledger.py", VISIBLE_TESTS, "test_ledger")


def write_fixture(wd: Path) -> None:
    wd.mkdir(parents=True, exist_ok=True)
    (wd / "TASK.md").write_text(TASK_MD)
    (wd / "ledger.py").write_text(STUB)
    (wd / "test_ledger.py").write_text(VISIBLE_TESTS)


# ── kickoffs ───────────────────────────────────────────────────────────────────
def kickoff_coder() -> str:
    return (
        f"{MARK} You are `{CODER}`, a programmer; your teammate `{REVIEWER}` reviews your "
        "work. Your working directory holds `TASK.md`, a stub `ledger.py` and "
        "`test_ledger.py`. Read and write files through `coder_fb` (a file agent: verbs "
        "`read` with `path`, `write` with `path` and `bytes`, `edit`). Run commands "
        "through `coder_run` (an exec runner in that directory): send it type `start` with "
        "`command` `python3` and `args` as items (`-m`, `unittest`, `-v`, `test_ledger`) — "
        "argv0 only in `command`, never a whole line. It returns a job_id at once; POLL "
        "type `status` with that job_id until it is no longer running, then read the "
        "output with type `tail`. Never end a turn with a job still running.\n\n"
        "Task: implement `parse` in `ledger.py` so it meets EVERY requirement in "
        "`TASK.md`. Never edit `test_ledger.py`. The visible tests cover only the happy "
        "path; the reviewer checks the whole contract.\n\n"
        f"Protocol: (1) implement, run the visible tests until green; (2) send `{REVIEWER}` "
        "a message (type `send`, `text`) whose text BEGINS with `REVIEW:` followed by a "
        "one-line summary — make it the LAST action of your turn, then end the turn with "
        "`done`; the reviewer's verdict arrives as a message that wakes you; (3) while "
        "you wait for the verdict do NOT touch `ledger.py`; (4) if the verdict starts with "
        "FAIL, fix EVERY numbered point (re-read `ledger.py` first), re-run the visible "
        "tests, and send `REVIEW:` again; (5) if it starts with PASS, send nothing more "
        f"and end with `done`. There are at most {CAP} REVIEW rounds. Begin now."
    )


def kickoff_reviewer() -> str:
    return (
        f"{MARK} You are `{REVIEWER}`, a code reviewer; `{CODER}` writes `ledger.py` and "
        "asks you to check it. You may READ the working directory through `reviewer_fb` "
        "(a READ-ONLY file agent: verb `read` with `path`; `TASK.md` is the contract) and "
        "run commands through `reviewer_run` (an exec runner in that directory: type "
        "`start` with `command` `python3` and `args` as items — argv0 only in `command` — "
        "then POLL type `status` with the job_id and read it with type `tail`). You may "
        "run the visible tests and your own one-off checks, e.g. `python3 -c \"...\"` "
        "snippets. NEVER write, move or delete any file, and never run git commands that "
        "change the tree.\n\n"
        "Wait for a message from the coder. On a message that begins with `REVIEW:` check "
        "`ledger.py` against EVERY requirement in `TASK.md`, not just the visible tests — "
        "those cover only the happy path. Read the code, then probe the edge cases "
        "yourself through your runner. Then reply to the coder with a message (type "
        "`send`, `text`) whose FIRST WORD is `PASS` or `FAIL`. After `FAIL`, give a "
        "numbered list; every point names `ledger.py` and a line number or function and "
        "says what is wrong and which requirement it breaks. Say `PASS` only when you "
        "have verified every requirement yourself; never pass work you have not checked. "
        "End each turn with `done`. Nothing to do yet: end this turn with `done` now."
    )


# ── the team ───────────────────────────────────────────────────────────────────
def _fb_rec(root, aid: str, owner: str, readonly: bool) -> dict:
    return {
        "type": "create_agent", "parent_id": home(root), "id": aid,
        "handler_module": "file_bridge.tools", "allow": ["."], "root": WORKDIR,
        "readonly": readonly, "ingress_rule": "allow_all_ingress",
        "description": f"The ledger workdir, {'READ-ONLY' if readonly else 'read-write'}, for `{owner}`.",
        "callable_by": {owner: "*", "rest": "*", "web": "*"},
    }


def _run_rec(root, aid: str, owner: str, cwd: str) -> dict:
    return {
        "type": "create_agent", "parent_id": home(root), "id": aid,
        "handler_module": "exec_bridge.tools", "cwd": cwd,
        "inherit_env": ["PATH", "HOME"], "ingress_rule": "allow_all_ingress",
        "allowed": ALLOWED,
        "description": f"Runner in the ledger workdir for `{owner}`: python3 and git only.",
        "callable_by": {owner: "*", "rest": "*", "web": "*"},
    }


def _ai_rec(root, bk, aid: str, peer: str, fb: str, run: str, description: str) -> dict:
    meta: dict = {
        "type": "create_agent", "parent_id": home(root), "id": aid,
        "description": description, "context_window": CTX,
        "can_call": {
            "$self": "*", fb: "*", run: "*", memory.MEMORY: "*",
            peer: ["send"],
        },
        "memory_store_id": memory.MEMORY,
        "callable_by": {"$self": "*", "rest": "*", "web": "*", peer: ["send"]},
    }
    # The plug describes ITSELF (e2e/backends); the probe's own wiring wins.
    for k, v in bk.record(role=aid).items():
        meta.setdefault(k, v)
    return meta


async def build(root, bk, cwd: str) -> None:
    recs = [
        _fb_rec(root, CODER_FB, CODER, False),
        _run_rec(root, CODER_RUN, CODER, cwd),
        _fb_rec(root, REVIEWER_FB, REVIEWER, True),
        _run_rec(root, REVIEWER_RUN, REVIEWER, cwd),
        _ai_rec(root, bk, CODER, REVIEWER, CODER_FB, CODER_RUN,
                "Coder: implements ledger.py, asks the reviewer, fixes what it reports."),
        _ai_rec(root, bk, REVIEWER, CODER, REVIEWER_FB, REVIEWER_RUN,
                "Reviewer: checks the coder's ledger.py against TASK.md; never edits."),
    ]
    out = await root.send([{"target": root.id, "payload": r} for r in recs])
    for o in out:
        assert not (o.get("result") or {}).get("error"), o
    for aid in (CODER, REVIEWER):  # the other half of each wire: the stores'
        await memory.open_ingress(root, memory.MEMORY, aid)
        if bk.key_ref:
            await creds.grant_reader(root, aid)


def _allows(grant, key, verb=None) -> bool:
    """Does the grant map admit `key` (for `verb`, when one is named)?"""
    if not isinstance(grant, dict):
        return False
    v = grant.get(key)
    if not v:
        return False
    if verb is None or v == "*":
        return True
    return isinstance(v, (list, tuple)) and (verb in v or "*" in v)


async def stored_records(root) -> dict:
    """The kernel's STORED record of every agent (list_agents) — not what we sent."""
    listed = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[0]
    return {a.get("id"): a for a in ((listed.get("result") or {}).get("agents") or [])}


def _refused_readonly(reply: dict) -> bool:
    """The READONLY refusal specifically — not an authz denial, not a path error."""
    return bool(reply.get("error")) and "readonly" in json.dumps(reply, default=str).lower()


async def check_wiring(root, bk) -> tuple[bool, list[str]]:
    """Every grant, BOTH halves, read from the stored records; the absences; the
    reviewer bridge's readonly refusal (with a read-write control)."""
    notes: list[str] = []
    recs = await stored_records(root)
    problems: list[str] = []
    for aid in IDS + [memory.MEMORY] + ([creds.CREDS] if bk.key_ref else []):
        r = recs.get(aid)
        if r is None:
            problems.append(f"{aid}: no stored record")
        elif not isinstance(r.get("can_call"), dict) and not isinstance(r.get("callable_by"), dict):
            problems.append(f"{aid}: stored record carries no grant maps")

    def can(aid):
        return (recs.get(aid) or {}).get("can_call")

    def by(aid):
        return (recs.get(aid) or {}).get("callable_by")

    wires = []  # (caller, callee, verb) — each needs caller.can_call AND callee.callable_by
    for me, peer, fb, run in ((CODER, REVIEWER, CODER_FB, CODER_RUN),
                              (REVIEWER, CODER, REVIEWER_FB, REVIEWER_RUN)):
        wires += [(me, peer, "send"), (me, fb, None), (me, run, None), (me, memory.MEMORY, None)]
        if bk.key_ref:
            wires.append((me, creds.CREDS, None))
    missing = []
    for caller, callee, verb in wires:
        if not _allows(can(caller), callee, verb):
            missing.append(f"{caller}.can_call[{callee}]")
        if not _allows(by(callee), caller, verb):
            missing.append(f"{callee}.callable_by[{caller}]")
    # nothing else: an AI's can_call names only its own wires; the other lane's
    # bridge and runner are granted from neither side
    leaks = []
    for me, peer, fb, run in ((CODER, REVIEWER, CODER_FB, CODER_RUN),
                              (REVIEWER, CODER, REVIEWER_FB, REVIEWER_RUN)):
        allowed = {"$self", peer, fb, run, memory.MEMORY} | ({creds.CREDS} if bk.key_ref else set())
        leaks += [f"{me}.can_call[{k}]" for k in (can(me) or {}) if k not in allowed]
    for a, b in ((CODER, REVIEWER_FB), (CODER, REVIEWER_RUN), (REVIEWER, CODER_FB), (REVIEWER, CODER_RUN)):
        if _allows(can(a), b) or _allows(by(b), a):
            leaks.append(f"{a}<->{b}")
    notes.append(f"stored records: {problems or 'ok'}; missing halves: {missing or 'none'}; extra reach: {leaks or 'none'}")

    # the reviewer's bridge: a read works, a write is refused AS READONLY. A control
    # write on the read-write bridge must succeed, so an authz denial of the harness
    # itself cannot pass for a readonly refusal.
    async def call(target, payload):
        return (await root.send([{"target": target, "payload": payload}]))[0].get("result") or {}

    rd = await call(REVIEWER_FB, {"type": "read", "path": "TASK.md"})
    wr = await call(REVIEWER_FB, {"type": "write", "path": "forbidden.txt", "bytes": "x"})
    ctl = await call(CODER_FB, {"type": "write", "path": ".wiring_probe", "bytes": "x"})
    if not ctl.get("error"):
        await call(CODER_FB, {"type": "delete", "path": ".wiring_probe"})
    refused = _refused_readonly(wr) and not (Path(WORKDIR) / "forbidden.txt").exists()
    notes.append(f"reviewer_fb read ok={not rd.get('error')}; write refused as readonly={refused} "
                 f"({str(wr.get('error'))[:60]}); control write on coder_fb ok={not ctl.get('error')}")
    ok = (not problems and not missing and not leaks and not rd.get("error")
          and refused and not ctl.get("error"))
    return ok, notes


# ── reading the run ────────────────────────────────────────────────────────────
async def _rows(root, aid):
    return await memory.rows_of(root, aid, conversation_only=True)


def _sends_to(rows, target: str) -> list[tuple[float, str, str]]:
    """(ts, row id, text) of every `send` an agent's assistant rows addressed to
    `target` — parsed by the kernel's own parser (helpers.ctx_score.calls_made), so a
    call the model only described does not count and a rendering difference between
    runtimes cannot empty the list."""
    from helpers.ctx_score import calls_made  # lazy: the self-test needs no kernel libs

    out = []
    for r in rows:
        for c in calls_made([r], target=target, verb="send"):
            out.append((float(r.get("ts") or 0), str(r.get("id")), str(c["payload"].get("text") or "").strip()))
    return out


def _is_incoming(r) -> bool:
    c = str(r.get("content") or "")
    return r.get("role") == "user" and MARK not in c and not c.lstrip().startswith("[reply to")


def _verdict_word(text: str) -> str | None:
    m = re.match(r"\W*(PASS|FAIL)\b", text)
    return m.group(1) if m else None


def _is_review(text: str) -> bool:
    return text.lstrip().startswith("REVIEW:")


def _pair(reviews: list[float], verdicts: list[float]) -> list[int | None]:
    """For each verdict ts: the index of the latest review whose ts is not after it
    (None when no review precedes it). `reviews` is in send order."""
    out: list[int | None] = []
    for vt in verdicts:
        idx = [i for i, rt in enumerate(reviews) if rt <= vt]
        out.append(idx[-1] if idx else None)
    return out


def _points(text: str) -> list[str]:
    return re.split(r"(?:^|\s)\d{1,2}[.)]\s+", text)[1:]


def _concrete(text: str, snapshot: str | None) -> bool:
    """At least one numbered point names ledger.py and a line number or a function."""
    names = {"parse"}
    try:
        names |= {n.name for n in ast.walk(ast.parse(snapshot or "")) if isinstance(n, ast.FunctionDef)}
    except SyntaxError:
        pass
    for p in _points(text):
        if "ledger.py" not in p:
            continue
        if re.search(r"\bline\s*\d+|:\d+|\bL\d+\b", p) or any(re.search(rf"\b{re.escape(n)}\b", p) for n in names):
            return True
    return False


async def _usage(root, aid: str) -> str:
    res = (await root.send([{"target": aid, "payload": {"type": "usage"}}]))[0].get("result") or {}
    if res.get("error"):
        return f"n/a ({str(res['error'])[:60]})"
    nums = {k: v for k, v in res.items() if isinstance(v, (int, float)) and not isinstance(v, bool)}
    if not nums:
        return f"n/a (no counters: {sorted(res)[:6]})"
    first = [k for k in ("requests", "prompt_tokens", "cache_read", "cache_write", "hit_rate", "output") if k in nums]
    rest = [k for k in nums if k not in first]
    return " ".join(f"{k}={nums[k]:.2f}" if isinstance(nums[k], float) else f"{k}={nums[k]}" for k in first + rest)


def _kick_fault(t: asyncio.Task) -> str | None:
    """A kickoff that FAILED is the harness's fault, never the model's."""
    if not t.done() or t.cancelled():
        return None
    e = t.exception()
    if e is not None:
        return f"kickoff raised {e!r}"
    out = t.result()
    r = (out[0].get("result") if out else None) or {}
    if isinstance(r, dict) and r.get("error"):
        return f"kickoff refused: {str(r['error'])[:160]}"
    return None


def _read(p: Path) -> str:
    return p.read_text(errors="ignore") if p.exists() else ""


def selftest() -> int:
    cases = [
        ("reference: visible", run_visible(REFERENCE), True),
        ("reference: oracle", run_oracle(REFERENCE), True),
        ("stub: visible", run_visible(STUB), False),
        ("stub: oracle", run_oracle(STUB), False),
        ("naive: visible", run_visible(NAIVE), True),
        ("naive: oracle", run_oracle(NAIVE), False),
    ]
    bad = 0
    for name, (ok, failed), want in cases:
        good = ok == want
        bad += not good
        print(f"  {'OK' if good else 'WRONG'}  {name:20} passed={ok} (want {want})"
              + (f" failing={len(failed)}" if failed else ""))
    logic = [
        ("pair: latest review before the verdict", _pair([1.0, 5.0, 9.0], [6.0, 10.0]) == [1, 2]),
        ("pair: a verdict with no review before it", _pair([4.0], [3.0]) == [None]),
        ("pair: same ts counts as before", _pair([2.0], [2.0]) == [0]),
        ("verdict word", _verdict_word("PASS - all good") == "PASS" and _verdict_word("  FAIL\n1. x") == "FAIL"
         and _verdict_word("I think PASS") is None),
        ("review text", _is_review("REVIEW: done") and not _is_review("please REVIEW: it")),
        ("concrete: names file and line", _concrete("FAIL\n1. ledger.py line 12 ignores '+'", REFERENCE)),
        ("concrete: names file and function", _concrete("FAIL 1. ledger.py parse() uses float", None)),
        ("concrete: no file named", not _concrete("FAIL 1. line 3 is wrong", None)),
        ("concrete: no number, no function", not _concrete("FAIL 1. ledger.py is bad", None)),
        ("readonly refusal shape", _refused_readonly({"error": "agent is readonly", "hint": "reads only"})
         and not _refused_readonly({"error": "caller not allowed"}) and not _refused_readonly({})),
        ("grant check", _allows({"a": ["send"]}, "a", "send") and _allows({"a": "*"}, "a", "send")
         and not _allows({"a": ["read"]}, "a", "send") and not _allows({}, "a")),
    ]
    for name, good in logic:
        bad += not good
        print(f"  {'OK' if good else 'WRONG'}  {name}")
    print("selftest:", "OK" if not bad else f"{bad} wrong")
    return 1 if bad else 0


async def main() -> None:
    bk = _resolve_backend()
    print("=" * (W + 4))
    print(f"  REVIEW LANE · {bk.name}/{bk.model} · round cap {CAP} · window {UNATTENDED:.0f}s")
    print(f"  models: {CODER} -> {bk.model_for(CODER)} · {REVIEWER} -> {bk.model_for(REVIEWER)}"
          "  (FANTASTIC_AI_MODEL_<ROLE> overrides the default)")
    print("=" * (W + 4))
    root, core, close = await open_kernel("review_lane_", reach=IDS)
    try:
        wd = Path(WORKDIR)
        ledger = wd / "ledger.py"
        write_fixture(wd)
        await build(root, bk, str(wd.resolve()))
        c1, wiring_notes = await check_wiring(root, bk)
        for n in wiring_notes:
            print(f"  · {n}")

        t0 = time.monotonic()
        kicks = [
            asyncio.create_task(root.send([{
                "target": aid,
                "payload": {"type": "send", "text": text, "client_id": aid},
                "timeout": UNATTENDED,
            }]))
            for aid, text in ((REVIEWER, kickoff_reviewer()), (CODER, kickoff_coder()))
        ]
        print(f"  kicked off {REVIEWER} then {CODER}; watching up to {UNATTENDED:.0f}s\n")

        # reviews: the coder's `REVIEW:` sends, each with ledger.py as it stood when
        # first seen (the coder waits for the verdict, so it is stable until then).
        # verdicts: the reviewer's PASS/FAIL sends, each with ledger.py AGAIN when seen.
        reviews: list[dict] = []
        verdicts: list[dict] = []
        seen: set = set()
        seen_rows: set = set()
        state = {"rrows": [], "crows": []}

        async def scan() -> None:
            crows, rrows = await _rows(root, CODER), await _rows(root, REVIEWER)
            state["crows"], state["rrows"] = crows, rrows
            for ts, rid, text in _sends_to(crows, REVIEWER):
                if _is_review(text) and ("R", rid, text) not in seen:
                    seen.add(("R", rid, text))
                    reviews.append({"ts": ts, "rid": rid, "text": text, "snap": _read(ledger)})
                    print(f"  · {time.monotonic() - t0:5.0f}s REVIEW #{len(reviews)} sent — ledger.py snapshot taken", flush=True)
            for ts, rid, text in _sends_to(rrows, CODER):
                word = _verdict_word(text)
                if word and ("V", rid, text) not in seen:
                    seen.add(("V", rid, text))
                    verdicts.append({"ts": ts, "rid": rid, "word": word, "text": text, "snap": _read(ledger)})
                    print(f"  · {time.monotonic() - t0:5.0f}s verdict {word} sent — ledger.py snapshot taken", flush=True)
            for who, rows in ((CODER, crows), (REVIEWER, rrows)):
                for r in rows:
                    if r.get("id") in seen_rows:
                        continue
                    seen_rows.add(r.get("id"))
                    body = " ".join(str(r.get("content") or "").split())
                    print(f"      {who:8} {str(r.get('role') or r.get('kind')):9} {body[:200]}", flush=True)

        watch = QuietWatch(root)
        over_cap = False
        fault = None
        while time.monotonic() - t0 < UNATTENDED:
            await asyncio.sleep(POLL)
            await scan()
            fault = next((f for f in map(_kick_fault, kicks) if f), None)
            if fault:
                print(f"  · HARNESS FAULT: {fault}", flush=True)
                break
            if len(reviews) > CAP:
                over_cap = True
                print(f"  · more than {CAP} REVIEW rounds — stopping", flush=True)
                break
            crows = state["crows"]
            last = crows[-1] if crows else None
            # settled: the latest verdict is PASS and the coder's last row is its closing
            # `done` turn (not a REVIEW) — no sleep, the row itself is the signal
            if (verdicts and verdicts[-1]["word"] == "PASS" and last and last.get("role") == "assistant"
                    and DONE_RE.search(str(last.get("content") or ""))
                    and not any(_is_review(t) for _, _, t in _sends_to([last], REVIEWER))):
                print(f"  · coder ended with done after PASS", flush=True)
                break
            if await watch.quiet():
                print(f"  · nothing in flight for {IDLE_END:.0f}s — the pair stopped", flush=True)
                break
        await scan()  # whatever landed since the last tick
        if fault is None and time.monotonic() - t0 < UNATTENDED - 5:
            fault = next((f for f in map(_kick_fault, kicks) if f), None)
        for t in kicks:
            if not t.done():
                t.cancel()

        # ── score ──
        crows, rrows = state["crows"], state["rrows"]
        final = _read(ledger)
        final_ok, final_failed = run_oracle(final)
        # ORDER: the rows' `ts` where every row carries one; otherwise their ids, which
        # the kernel mints from one rising counter for every agent (rust rows carry no ts).
        def _order(x):
            return x["ts"] if all(y["ts"] > 0 for y in reviews + verdicts) else float(x["rid"])
        try:
            pairing = _pair([_order(r) for r in reviews], [_order(v) for v in verdicts])
            ts_missing = False
        except (TypeError, ValueError):
            pairing, ts_missing = [None] * len(verdicts), True
        oracle_at_review = [run_oracle(r["snap"]) for r in reviews]
        oracle_at_verdict = [run_oracle(v["snap"]) for v in verdicts]
        received = sum(1 for r in rrows if _is_incoming(r) and "REVIEW:" in str(r.get("content") or ""))

        print("\n" + "=" * (W + 4))
        if fault or ts_missing:
            print(f"    HARNESS FAULT — not a model result: {fault or 'rows carry neither a ts nor a numeric id, verdicts cannot be paired'}")
        print(f"    1. team built and wired              {'OK' if c1 else 'NO'}")
        c2 = bool(reviews) and received >= 1 and bool(verdicts)
        print(f"    2. a review round happened           {'OK' if c2 else 'NO'} reviews sent={len(reviews)} "
              f"received by reviewer={received} verdicts={len(verdicts)}")
        print(f"    3. oracle passes on final ledger.py  {'OK' if final_ok else 'NO'} failing={final_failed[:4]}")
        broken = []
        for i, v in enumerate(verdicts):
            if v["word"] != "PASS":
                continue
            j = pairing[i]
            if j is None or not oracle_at_verdict[i][0] or not oracle_at_review[j][0]:
                broken.append(i + 1)
        c4 = c2 and not broken
        print(f"    4. reviewer never passed broken work {'OK' if c4 else 'NO'} broken PASS (verdict #) {broken or 'none'}")
        fails = [i for i, v in enumerate(verdicts) if v["word"] == "FAIL"]
        weak = [i + 1 for i in fails
                if not _concrete(verdicts[i]["text"], reviews[pairing[i]]["snap"] if pairing[i] is not None else None)]
        c5 = c2 and not weak
        print(f"    5. review points are concrete        {'OK' if c5 else 'NO'} FAIL replies={len(fails)}"
              f"{' (vacuous: none)' if not fails else ''} vague={weak or 'none'}")
        last_assistant = next((r for r in reversed(crows) if r.get("role") == "assistant"), None)
        ended_done = bool(last_assistant and DONE_RE.search(str(last_assistant.get("content") or ""))
                          and not any(_is_review(t) for _, _, t in _sends_to([last_assistant], REVIEWER)))
        got_pass = any(v["word"] == "PASS" for v in verdicts)
        c6 = ended_done and got_pass and len(reviews) <= CAP and not over_cap
        print(f"    6. coder ended with done in cap      {'OK' if c6 else 'NO'} done={ended_done} pass={got_pass} rounds={len(reviews)}/{CAP}")
        # Only a PASS must find the file where the REVIEW left it. After a FAIL the
        # coder may legitimately start fixing before the verdict snapshot is taken,
        # so FAIL verdicts are not compared.
        moved = [i + 1 for i, v in enumerate(verdicts)
                 if v["word"] == "PASS" and pairing[i] is not None and v["snap"] != reviews[pairing[i]]["snap"]]
        orphan = [i + 1 for i, p in enumerate(pairing) if p is None]
        c7 = c2 and not moved and not orphan
        print(f"    7. ledger.py still under a PASS      {'OK' if c7 else 'NO'} changed between REVIEW and a PASS (verdict #) "
              f"{moved or 'none'}; verdicts with no REVIEW before them {orphan or 'none'}")

        print("\n  ── the exchange (in order) ──")
        events = ([(r["ts"], CODER, REVIEWER, r["text"]) for r in reviews]
                  + [(v["ts"], REVIEWER, CODER, v["text"]) for v in verdicts])
        base = min((e[0] for e in events), default=0.0)
        for ts, frm, to, txt in sorted(events, key=lambda e: e[0]):
            print(f"  {ts - base:6.0f}s {frm} → {to}: {' '.join(txt.split())[:150]}")
        print(f"\n  rounds: {len(reviews)} (cap {CAP})")
        print(f"  note: snapshots are taken on a {POLL:.0f}s poll, so each can lag its message by up to {POLL:.0f}s")
        print("  per verdict — oracle on ledger.py at that verdict vs the reviewer's word:")
        for i, v in enumerate(verdicts):
            ok, failed = oracle_at_verdict[i]
            print(f"    verdict {i + 1} (review {('#' + str(pairing[i] + 1)) if pairing[i] is not None else '-'}): "
                  f"oracle={'PASS' if ok else 'FAIL'} reviewer={v['word']}" + (f" failing={failed[:3]}" if failed else ""))
        print("  per REVIEW — oracle on the ledger.py the coder asked about:")
        for i, (ok, failed) in enumerate(oracle_at_review):
            print(f"    review {i + 1}: oracle={'PASS' if ok else 'FAIL'}" + (f" failing={failed[:3]}" if failed else ""))
        print("  per lane — usage:")
        for lane in (CODER, REVIEWER):
            print(f"    {lane:9} [{bk.model_for(lane)}]: {await _usage(root, lane)}")
        print(f"  wall: {time.monotonic() - t0:.0f}s")

        harness_ok = not fault and not ts_missing
        ok = harness_ok and c1 and c2 and final_ok and c4 and c5 and c6 and c7
        if not harness_ok:
            print("\n  RESULT: HARNESS FAULT — the model was not scored")
        else:
            print(f"\n  RESULT: {'PASS — the reviewer held the line and the coder fixed what it found' if ok else 'FAIL'}")
        verdict(ok)
        sys.exit(0 if ok else (2 if not harness_ok else 1))
    finally:
        await close()


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        sys.exit(selftest())
    asyncio.run(main())
