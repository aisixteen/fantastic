"""agentic_advanced/pause_resume — a paused agent holds its messages across a restart.

Python runtime, NO MODEL (a stub provider). Turn 0 is answered; then `pause`, three
messages sent while paused (each held, none reaches the model), a kernel RESTART (the
pause and the queue live in the memory store), a fourth message, `resume`. Scored: the
replies of each verb, the model seeing no held text while paused or at boot, the first
request after the resume carrying the resumed row then the four messages in order, and
the store holding pause, held x4, resume, then each message once as a user row.
"""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from helpers import memory, verdict
from helpers.ai import _live_tasks
from helpers.kernel import WRAPS, home, open_kernel, replace_with, restart

MSGS = ["m1-probe", "m2-probe", "m3-probe", "m4-probe"]


class _Stub:
    last_prompt_tokens = None
    seen: list = []

    async def chat(self, messages, max_tokens=None):
        _Stub.seen.append(json.dumps(messages))
        _Stub.msgs.append([str(m.get("content")) for m in messages])
        yield '<call name="done"></call>'

    msgs: list = []


async def _call(root, payload):
    return (await root.send([{"target": "ai", "payload": payload}]))[0]["result"]


async def _quiet(root):
    quiet = 0
    for _ in range(400):
        await asyncio.sleep(0.05)
        quiet = quiet + 1 if not await _live_tasks(root, "ai") else 0
        if quiet >= 6:
            return
    raise AssertionError("the chain never went quiet")


def _mentions(text):
    return [m for m in MSGS + ["m0-probe"] if m in text]


async def main():
    root, core, close = await open_kernel(prefix="agentic_pause_", reach=["ai"])
    try:
        WRAPS["ai"] = replace_with(_Stub())
        r = (await root.send([{"target": root.id, "payload": {
            "type": "create_agent", "parent_id": home(root), "id": "ai",
            "handler_module": "ollama_backend.tools", "model": "stub",
            "endpoint": "http://stub.invalid",  # never dialled: WRAPS seats the stub
            "context_window": 32000, **memory.ai_memory_meta({"cli": "*"})}}]))[0]["result"]
        assert not r.get("error"), r
        await memory.open_ingress(root, memory.MEMORY, "ai")

        await _call(root, {"type": "send", "text": "m0-probe"})
        await _quiet(root)
        n0 = len(_Stub.seen)
        paused = await _call(root, {"type": "pause"})
        sends = [await _call(root, {"type": "send", "text": t}) for t in MSGS[:3]]
        await asyncio.sleep(0.5)
        leaked = [s for s in _Stub.seen[n0:] if any(t in s for t in MSGS)]
        n1 = len(_Stub.seen)

        root2, _stop = await restart(root, core)
        WRAPS["ai"] = replace_with(_Stub())
        await asyncio.sleep(0.5)
        boot_leak = [s for s in _Stub.seen[n1:] if any(t in s for t in MSGS)]
        again = await _call(root2, {"type": "pause"})
        fourth = await _call(root2, {"type": "send", "text": MSGS[3]})
        await asyncio.sleep(0.3)
        held_still = len(_Stub.seen) == n1
        resumed = await _call(root2, {"type": "resume"})
        await _quiet(root2)
        noop = await _call(root2, {"type": "resume"})

        first = next((m for m in _Stub.msgs if any(MSGS[3] in c for c in m)), [])

        def at(needle):
            return next((i for i, c in enumerate(first) if needle in c), -1)

        pos = [at("resumed after a pause")] + [at(t) for t in MSGS]
        rows = await memory.rows_of(root2, "ai")
        kinds = [r.get("kind") for r in rows if r.get("kind") in ("pause", "held", "resume")]
        held_txt = [str((r.get("payload") or {}).get("text")) for r in rows if r.get("kind") == "held"]
        users = [r for r in rows if r.get("role") == "user" and r.get("kind") in (None, "message")]
        user_ids = {t: [r["id"] for r in users if t in str(r.get("content"))] for t in MSGS}
        resume_id = next((r["id"] for r in rows if r.get("kind") == "resume"), None)
        print(f"     paused={paused} sends={sends} again={again} fourth={fourth} "
              f"resumed={resumed} noop={noop} pos={pos} kinds={kinds}")
        checks = [
            ("pause answers paused with nothing held", paused == {"paused": True, "held": 0}),
            ("each message sent while paused is held in order",
             [(s.get("held"), s.get("paused")) for s in sends] == [(1, True), (2, True), (3, True)]),
            ("no held message reached the model while paused", not leaked),
            ("no held message reached the model at the restart", not boot_leak),
            ("after the restart the pause and 3 held survive",
             again == {"paused": True, "held": 3}),
            ("a new message is held as the 4th", (fourth.get("held"), fourth.get("paused")) == (4, True)
             and held_still),
            ("resume releases all four", resumed == {"resumed": True, "released": 4}),
            ("the first request after the resume carries the resumed row then m1..m4 in order",
             all(p >= 0 for p in pos) and pos == sorted(pos) and len(set(pos)) == 5),
            ("the store holds pause, held x4, resume in order",
             kinds == ["pause"] + ["held"] * 4 + ["resume"]
             and held_txt == MSGS),
            ("each message is a user row exactly once, after the resumed row",
             all(len(v) == 1 and resume_id is not None and v[0] > resume_id for v in user_ids.values())),
            ("resume when not paused is a no-op", noop == {"resumed": False, "released": 0}),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "pause held four messages across a restart; resume released them in order"
                if allok else "a pause_resume check failed")
    finally:
        WRAPS.pop("ai", None)
        await close()


if __name__ == "__main__":
    asyncio.run(main())
