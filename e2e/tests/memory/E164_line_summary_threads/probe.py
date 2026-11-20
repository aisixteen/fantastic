"""memory/line_summary_threads (#709) — THREADS: across a `context_clear` (a new thread), each
thread's stretches fold under their own lines, and nothing leaks between threads. NO MODEL: a
scripted vLLM mock plays the model.

Rust lane only. The rig of E163 (`helpers/line_fold.py`): one `ai_chat.tools` chat with
`line_summaries` on, folding over a 6000 token window, watched over the ws face.

  A1  thread A: each answer writes `line_summary` A-<k> until a fold lands; its title is the
      latest A line its members write;
  C   the person clears the context (`context_clear`): a new thread starts at the clear row;
  B1  thread B: answers write NO line until a fold lands: that fold has no line, so it keeps the
      summariser's title (asked for a title line), never an A line, and holds no A row;
  B2  thread B goes on, each answer writing `line_summary` B-<k>, until a second fold lands: its
      title is the latest B line its members write, and it holds no A row;
  V   the view: the chat published every line of both threads, each once and in order;
      reflect returns the latest B line.
"""

from __future__ import annotations

import os
import secrets
import shutil
import sys
import tempfile
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import ensure_runtime, runtime
from helpers.line_fold import (AI, BODY_ASK, DONE, TITLED_ASK, Rig, filler, line)
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
TOKEN = secrets.token_hex(4)
BODY = f"ASKED: the steps. DONE: noted BODY-{TOKEN}. BINDING: none. ERRORS: none. OPEN: none."
TITLE = f"SUMTITLE-{TOKEN}"


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-line-threads-"))
    rig = Rig(spec, work, BODY, TITLE)
    need = rig.need
    try:
        rig.wire()
        written: list[str] = []
        known: set = set()

        def lined(tag: str):
            def step(k: int) -> tuple[str, str]:
                text = f"{tag}-{TOKEN}-{k}"
                written.append(text)
                return (f"{tag} step {k}: {filler(f'{tag}{k}')}", f"{line(text)}\n{tag} step {k} noted.\n{DONE}")
            return step

        def a_member(f: dict, cleared: int) -> list[int]:
            """The members of fold `f` from thread A: row ids come from the kernel's one
            counter, so every row of A is older than the clear row."""
            return [m["id"] for m in f["members"] if m["id"] < cleared]

        # ── A1 · thread A folds under its own line ────────────────────────────
        fa = rig.until_fold("A1", known, lined("A"))
        if fa is None:
            raise SystemExit(verdict(False, f"{RUNTIME}: no fold in thread A"))
        a_lines = list(written)
        f = rig.fold(fa)
        want = rig.last_line(f["members"])
        need(want is not None and want in a_lines and f["title"] == want,
             f"A1: thread A's fold is titled by its latest line: {f['title']!r} (latest {want!r})")

        # ── C · a new thread ──────────────────────────────────────────────────
        r = rig.hk.send(AI, {"type": "context_clear", "why": "a new thread"})
        cleared = r.get("cleared")
        need(isinstance(cleared, int), f"C: the context is cleared: {str(r)[:200]}")
        head = rig.history()
        if not isinstance(cleared, int):
            raise SystemExit(verdict(False, f"{RUNTIME}: no clear row"))
        need(not any(isinstance(x.get("id"), int) and x["id"] < cleared for x in head),
             f"C: the new thread holds none of thread A's rows: {[x.get('id') for x in head][:12]}")

        # ── B1 · thread B, no lines: its fold keeps the summariser's title ─────
        titled_before = len(rig.asks(TITLED_ASK))

        def bare(k: int) -> tuple[str, str]:
            return (f"B0 step {k}: {filler(f'B0{k}')}", f"B0 step {k} noted.\n{DONE}")

        fb1 = rig.until_fold("B1", known, bare)
        if fb1 is None:
            raise SystemExit(verdict(False, f"{RUNTIME}: no fold in thread B"))
        f = rig.fold(fb1)
        need(rig.last_line(f["members"]) is None, "B1: the stretch writes no line")
        need(f["title"] == TITLE, f"B1: the fold keeps the summariser's title: {f['title']!r}")
        need(not any(f["title"] == t for t in a_lines), "B1: no line of thread A titles it")
        need(len(rig.asks(TITLED_ASK)) > titled_before, "B1: the summariser was asked for a title line")
        need(a_member(f, cleared) == [], f"B1: the fold holds no row of thread A: {a_member(f, cleared)}")

        # ── B2 · thread B with lines: its own line titles its fold ────────────
        b_from = len(written)
        fb2 = rig.until_fold("B2", known, lined("B"))
        if fb2 is None:
            raise SystemExit(verdict(False, f"{RUNTIME}: no second fold in thread B"))
        b_lines = written[b_from:]
        f = rig.fold(fb2)
        want = rig.last_line(f["members"])
        print(f"  info B2: fold {fb2}: {f['refs']} refs, members {[m['id'] for m in f['members']]}",
              flush=True)
        need(want is not None and want in b_lines and f["title"] == want,
             f"B2: thread B's fold is titled by its latest line: {f['title']!r} (latest {want!r})")
        need(a_member(f, cleared) == [], f"B2: the fold holds no row of thread A: {a_member(f, cleared)}")
        body_asks = rig.asks(BODY_ASK)
        need(len(body_asks) >= 2, f"B2: both lined folds asked for the body alone: {len(body_asks)}")

        # ── V · the view ───────────────────────────────────────────────────────
        lines = rig.chat_ev.of("line_summary_changed")
        need([e.get("text") for e in lines] == written,
             f"V: the chat published every line once, in order: {len(lines)} for {len(written)}")
        got = rig.reflect_line()
        need(got.get("text") == written[-1], f"V: reflect returns the latest B line: {got}")
    finally:
        rig.close()
        shutil.rmtree(work, ignore_errors=True)

    bad = rig.bad
    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
