"""SMOKE — the llamacpp_backend bundle against a live `llama-server`.

    cd src/lib/python && uv run python ../../../tmp/live_llamacpp.py

Not an emergence probe. This asserts the ONE thing the bundle exists for: that a
turn spent reasoning is not read as an EMPTY completion. Two sends against the
same agent, differing only in the `think` record field:

  think=False  → no thought channel; `content` carries everything.
  think=True   → the model reasons, and BOTH channels survive: the thought arrives
                 wrapped once in <think>…</think> (parsed for tool calls, stripped
                 before persistence) and `content` is still non-empty.

The failure this guards against is silent: with the gate off and a provider that
reads only `content`, the model spends a whole pass thinking and you observe
nothing at all — no error, no text, no turn.
"""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth, so moving this
# file into a folder can never silently break its imports again.
sys.path.insert(0, str(next(d for d in Path(__file__).resolve().parents
                            if (d / ".e2e-root").exists())))
from helpers import (  # noqa: E402
    BASELINE, Limits, ProxyReader, Watch, distinct_calls, verdict,
)
from backends import backend as _resolve_backend  # noqa: E402

ENDPOINT = os.environ.get("FANTASTIC_LLAMACPP_ENDPOINT", "http://127.0.0.1:8080/v1")
TASK = "A farmer has 17 sheep. All but 9 run away. How many are left?"
W = 78


def box(title: str, body: str, mark: str = "|") -> None:
    print(f"\n{title}")
    for raw in (body or "(EMPTY)").splitlines() or [""]:
        for i in range(0, len(raw), W):
            print(f"  {mark} {raw[i : i + W]}")


async def main():
    os.chdir(tempfile.mkdtemp(prefix="llamacpp_"))
    from _testkit import boot_root

    root = boot_root()
    store = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "file_bridge.tools",
            "id": "kernel_store",
            "root": ".fantastic",
            "ingress_rule": "allow_all_ingress",
        }}]))[0]["result"]
    ai = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent",
            "parent_id": root.id,
            "handler_module": "llamacpp_backend.tools",
            "id": "ai",
            "endpoint": ENDPOINT,
            "file_bridge_id": store["id"],
            "num_predict": 400,
        }}]))[0]["result"]
    aid = ai["id"]

    rec = (await root.send([{"target": aid, "payload": {"type": "reflect"}}]))[0]["result"]
    print("=" * (W + 4))
    print(f"  {rec['sentence']}")
    print(f"  endpoint={ENDPOINT}  model={rec.get('model')}")
    print("=" * (W + 4))

    results = {}
    for think in (False, True):
        (await root.send([{"target": root.id, "payload": {"type": "update_agent", "id": aid, "think": think}}]))[0]["result"]
        reply = (await root.send([{"target": aid, "payload": {"type": "send", "text": TASK, "client_id": f"probe_{think}"}}]))[0]["result"]
        text = reply.get("response") or ""
        results[think] = text
        box(f"-- think={think} -> response ({len(text)} chars)", text[:900], ">")

    # THE THOUGHT CHANNEL LIVES ONE LAYER DOWN. `response` is post-strip by design
    # (ai_core.without_thinking removes the wrapper before the turn is persisted), so
    # asserting `<think> in response` tests the WRONG surface and fails on a healthy
    # system — it did, on 2026-08-04, the first time this script ran. The wrapper is
    # observable where it is produced: the provider stream.
    from llamacpp_backend.provider import LlamaCppProvider

    from ai_core.core import THINK_CLOSE, THINK_OPEN

    raw = {}
    for think in (False, True):
        p = LlamaCppProvider(endpoint=ENDPOINT, think=think, num_predict=400)
        raw[think] = "".join([c async for c in p.chat([{"role": "user", "content": TASK}])])
        await p.aclose()

    print("\n" + "=" * (W + 4))
    off, on = results[False], results[True]
    wrapped = THINK_OPEN in raw[True] and THINK_CLOSE in raw[True]
    quiet = THINK_OPEN not in raw[False]
    thought = raw[True].split(THINK_CLOSE)[0].replace(THINK_OPEN, "") if wrapped else ""
    after = raw[True].split(THINK_CLOSE)[-1] if wrapped else ""
    ok = bool(off.strip()) and bool(on.strip()) and wrapped and quiet and bool(after.strip())
    print(f"    kernel   think=False response non-empty : {bool(off.strip())} ({len(off)} ch)")
    print(f"    kernel   think=True  response non-empty : {bool(on.strip())} ({len(on)} ch)")
    print(f"    provider think=False no thought channel : {quiet}")
    print(f"    provider think=True  thought wrapped    : {wrapped} ({len(thought)} ch)")
    print(f"    provider think=True  content after it   : {bool(after.strip())} ({len(after)} ch)")
    print(f"\n  RESULT: {'PASS' if ok else 'FAIL'}")
    verdict(ok)


asyncio.run(main())
