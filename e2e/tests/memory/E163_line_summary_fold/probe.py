"""memory/line_summary_fold (#709) — NO THREADS: a chat's line_summary lines become its fold's
title, and the view agrees. NO MODEL: a scripted vLLM mock plays the model.

Rust lane only. One `ai_chat.tools` chat (`line_summaries` on, `context_strategy: fold`, a 6000
token window) completes through a `vllm_backend` provider on the mock. The chat's events are
watched over the ws face.

  L1  each scripted answer writes `line_summary` LINE-<k>: the chat publishes
      `line_summary_changed {text, ts}` for each, and its `reflect` returns the latest;
  F1  a fold lands in the one thread: its title is the latest line its members write, the
      summariser was asked for the body alone (no title line) and the fold's summary is that
      body; the next request shows the fold line `[fold N · K refs · L1] "LINE-<k>"`;
  F2  the view agrees: the fold's title is the line the chat published for that stretch.
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
from helpers.line_fold import (AI, BODY_ASK, DONE, TITLED_ASK, Rig, blob, filler, line)
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
TOKEN = secrets.token_hex(4)
BODY = f"ASKED: the steps. DONE: noted BODY-{TOKEN}. BINDING: none. ERRORS: none. OPEN: none."
TITLE = f"SUMTITLE-{TOKEN}"


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-line-fold-"))
    rig = Rig(spec, work, BODY, TITLE)
    need = rig.need
    try:
        rig.wire()
        written: list[str] = []

        def step(k: int) -> tuple[str, str]:
            text = f"LINE-{TOKEN}-{k}"
            written.append(text)
            return (f"step {k}: {filler(f's{k}')}", f"{line(text)}\nstep {k} noted.\n{DONE}")

        # ── L1 + F1: lines are written until a fold lands ──────────────────────
        fid = rig.until_fold("F1", set(), step)
        if fid is None:
            raise SystemExit(verdict(False, f"{RUNTIME}: no fold"))
        last = written[-1]
        got = rig.reflect_line()
        need(got.get("text") == last and isinstance(got.get("ts"), (int, float)),
             f"L1: reflect returns the latest line: {got}")
        need(rig.chat_ev.wait(lambda e: e.get("type") == "line_summary_changed" and e.get("text") == last),
             "L1: the chat published the latest line")
        lines = rig.chat_ev.of("line_summary_changed")
        need([e.get("text") for e in lines] == written and all(e.get("source") == AI for e in lines),
             f"L1: one event per line, in order, from the chat: {len(lines)} for {len(written)} lines")
        need(all(isinstance(e.get("ts"), (int, float)) for e in lines), "L1: every event carries its ts")

        f = rig.fold(fid)
        want = rig.last_line(f["members"])
        print(f"  info F1: fold {fid}: {len(f['members'])} members, title {f['title']!r}, "
              f"latest line among them {want!r}", flush=True)
        need(want is not None and want in written, "F1: the fold's members write lines")
        need(f["title"] == want, f"F1: the fold's title is the stretch's latest line: {f['title']!r}")
        body_asks = rig.asks(BODY_ASK)
        need(len(body_asks) >= 1 and not any(TITLED_ASK in a for a in body_asks),
             f"F1: the summariser was asked for the body alone: {len(body_asks)} body asks, "
             f"{len(rig.asks(TITLED_ASK))} titled")
        need(f["summary"].startswith(BODY), f"F1: the fold's summary is that body: {f['summary'][:120]!r}")
        # the next request shows the fold under that line
        rig.turn("one more", f"{line('after the fold')}\nnoted.\n{DONE}")
        seen = blob(rig.mock.turns()[-1])
        need(f'[fold {fid} · {len(f["members"])} refs · L1] \\"{want}\\"' in seen,
             f"F1: the next request shows the fold line under its title")

        # ── F2: the view agrees ────────────────────────────────────────────────
        need(want in [e.get("text") for e in lines], "F2: the fold's title is a line the chat published")
    finally:
        rig.close()
        shutil.rmtree(work, ignore_errors=True)

    bad = rig.bad
    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
