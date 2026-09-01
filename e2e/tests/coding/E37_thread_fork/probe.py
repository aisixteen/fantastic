"""coding/thread_fork — a forked agent inherits the pre-fork thread. LLM-driven.

Codex `fork_thread.rs` / `multi_agent_*`. A "thread" IS the JSONL sidecar
`agents/<id>/chat_<client>.jsonl` on the agent's leg. Fork = create a clone +
`pump` (server-side copy) the sidecar across. The clone wakes with the
original's full history and answers a question only that history can answer.
No new primitive — create_agent + pump + sidecars, which already exist. (The
operator orchestrates the fork here; the model-forks-itself variant is a
follow-on.)
"""

from __future__ import annotations

import asyncio
import os
import secrets
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, verdict
from helpers.kernel import open_kernel

BK = _resolve_backend()
FB = "fork_fb"
DONE = 'end your run by calling done — emit <tool_call>{"name":"done"}</tool_call>'


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def main():
    root, core, close = await open_kernel(
        prefix="coding_fork_", reach=[FB, "ai", "ai_fork", "cli"]
    )
    try:
        secret = secrets.token_hex(4)
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": root.id, "id": FB,
            "handler_module": "file_bridge.tools", "root": ".",
            "ingress_rule": "allow_all_ingress",
            "callable_by": {"ai": "*", "ai_fork": "*", "rest": "*", "web": "*"},
        })
        assert not r.get("error"), f"create {FB}: {r}"

        # ── the ORIGINAL learns a fact (persisted to its sidecar) ──
        a = await connect(root, core, BK, agent_id="ai",
                          can_call={"cli": "*"},
                          limits=Limits.from_env(os.environ), record={"file_bridge_id": FB})
        await drive(a,
            f"Remember this for later — the vault code is `{secret}`. "
            f"Acknowledge that you have it, then {DONE}.", client_id="c")
        await a.close()

        src = "agents/ai/chat_c.jsonl"
        dst = "agents/ai_fork/chat_c.jsonl"
        # the sidecar lives under the leg's root (cwd = "."); path is store-relative
        # to FB. Confirm the original wrote it (through the leg, not the disk guess).
        rd = await _call(root, FB, {"type": "list", "path": "agents/ai"})

        # ── FORK: clone the agent + pump its thread into the clone's sidecar ──
        made = await _call(root, root.id, {
            "type": "create_agent", "parent_id": root.id, "id": "ai_fork",
            **BK.record(),
            "file_bridge_id": FB, "context_window": 32768,
            "can_call": {"cli": "*"},
            "callable_by": {"$self": "*", "$parent": "*", "$children": "*",
                            "rest": "*", "web": "*"},
        })
        assert not made.get("error"), f"fork create: {made}"
        pumped = await _call(root, FB, {
            "type": "pump", "source": FB, "source_path": src,
            "sink": FB, "sink_path": dst,
        })
        assert not pumped.get("error"), f"pump: {pumped}"

        # ── the CLONE answers from the inherited thread (same client_id) ──
        # ai_fork already exists; drive it as OPERATOR (one send) and read back
        # its history — it loads the PUMPED sidecar + this question, and can only
        # answer from the thread it inherited.
        _ = await _call(root, "ai_fork", {
            "type": "send",
            "text": ("What vault code did I give you earlier in this "
                     "conversation? Reply with just the code."),
            "client_id": "c",
        })
        await asyncio.sleep(1.0)
        hist = await _call(root, "ai_fork", {"type": "history", "client_id": "c"})
        joined = str(hist.get("messages", hist))

        listing = str(rd)
        checks = [
            ("the original wrote its thread sidecar", "chat_c.jsonl" in listing),
            ("the fork is a distinct agent", made.get("id") == "ai_fork"),
            ("the fork inherited the fact from the pumped thread", secret in joined),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "forked the thread — the clone knows the pre-fork fact"
                if allok else "a thread_fork check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
