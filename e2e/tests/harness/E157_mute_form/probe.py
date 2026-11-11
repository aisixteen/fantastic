"""harness/mute_form — the launcher's MUTE FORM, with no model and no kernel.

A scenario or test declares `muted: "#<issue>: <one-line reason>"`. `run.py` then
  a) does not run it: its command is one that would fail loudly if it were started;
  b) reports it SKIP with the text `muted #<issue>: <reason>`, counted with the skips;
  c) refuses a mute that is not of that form (no issue, no reason);
  d) reads the key from both declaration files the suite loads (`test.yaml`, `scenario.yaml`)
     — here, the real suite: every `muted:` in the tree is well formed."""

from __future__ import annotations

import asyncio
import sys
import tempfile
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

import run  # noqa: E402
from helpers.verdict import verdict  # noqa: E402

bad: list[str] = []


def need(cond: bool, what: str) -> None:
    print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
    if not cond:
        bad.append(what)


def main() -> int:
    muted = {"group": "g", "id": "t:rust", "title": "T", "muted": "#616: waits for the redesign",
             "cmd": ["sh", "-c", "echo STARTED; exit 1"], "cwd_key": "e2e"}
    live = {"group": "g", "id": "u:rust", "title": "U"}

    # the note and the form
    need(run.mute_note(muted) == "muted #616: waits for the redesign", f"the note: {run.mute_note(muted)!r}")
    need(run.mute_note(live) is None, "a test with no `muted:` has no note")
    need(run.check_muted([muted, live]) == [], "a well-formed mute passes the form check")
    for form in ("", "no issue", "#: no number", "#12", "#12:", "#12:   ", 12, ["#1: x"]):
        need(len(run.check_muted([{**muted, "muted": form}])) == 1, f"refused: muted: {form!r}")

    # not run, reported SKIP with the text, counted as a skip
    with tempfile.TemporaryDirectory() as tmp:
        results: dict = {}
        asyncio.run(run.run_one(muted, run._NoBackend(), Path(tmp), asyncio.Semaphore(1), results))
        r = results.get("g.t:rust") or {}
        need(r.get("state") == "skip", f"state: {r.get('state')!r}")
        need(r.get("note") == "muted #616: waits for the redesign", f"note: {r.get('note')!r}")
        log = (Path(tmp) / run.log_name("g.t:rust")).read_text()
        need("STARTED" not in log, "its command was not started")
        need("muted #616: waits for the redesign" in log, "its log carries the text")

    # the real suite: the declaration files are read, every mute is well formed
    tests = run.load_suite()
    need(run.check_muted(tests) == [], f"every `muted:` in the suite is well formed: {run.check_muted(tests)}")
    names = [f"{t['group']}.{t['id']}" for t in tests if t.get("muted")]
    print(f"  muted in the suite: {names}", flush=True)

    verdict(not bad, f"{len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
