"""backends/stream_stop — a RECEIVER stops a live stream (owner, 2026-10-04). LLM-driven.

The AI is asked to write a long message to `rx`. Its call streams to `rx` as `part_*`
events (the stream junction, `helpers/stream.py`). Once the draft has some body, the
harness — acting AS `rx` — calls the AI's `stop`. Scored: the stop is acknowledged,
the draft ABORTS (never ends, never matched by a delivered message), the generation
settles cancelled, and the AI's newest assistant row is the marker
`[stopped by rx]` naming the call that was not sent — never the half answer.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import connect, memory, verdict
from helpers.kernel import open_kernel
from helpers.stream import StreamJunction

BK = _resolve_backend()

TASK = (
    "Write a LONG essay (about 800 words) about rivers and deliver it to the agent "
    "`rx` as ONE message, exactly in this shape:\n"
    '<call name="send">\n<target_id>rx</target_id>\n<type>note</type>\n'
    "<text>…the whole essay…</text>\n</call>\n"
    "Write the call first, then end with <call name=\"done\"></call>."
)


async def main():
    root, core, close = await open_kernel(prefix="stream_stop_", reach=["ai", "rx", "cli"])
    try:
        rec = root.create("ram_store.tools", id="rx")
        assert rec.get("id") == "rx", rec
        conn = await connect(
            root, core, BK, can_call={"rx": "*", "cli": "*"},
            callable_by={"rx": ["stop"]},
        )
        # rx's half of both edges: it accepts the AI's parts, and may call `stop`
        await memory.open_ingress(root, "rx", conn.id)
        cc = dict(root.ctx.agents["rx"]._meta.get("can_call") or {})
        cc[conn.id] = ["stop"]
        root.ctx.update("rx", can_call=cc)
        rx = StreamJunction.watch(root, "rx")
        turn = asyncio.ensure_future(
            root.send([{"target": conn.id, "payload": {"type": "send", "text": TASK}}])
        )
        stopped = None
        for _ in range(900):  # ≤ 180 s for the draft to grow
            await asyncio.sleep(0.2)
            rx.drain()
            draft = next(
                (d for d in rx.drafts.values() if d.kind == "call" and d.target == "rx"), None
            )
            if draft is not None and len(draft.text) >= 300:
                with root.ctx.agents["rx"]._as_self():
                    stopped = (await root.send(
                        [{"target": conn.id, "payload": {"type": "stop"}}]))[0]["result"]
                break
            if turn.done():
                break
        out = (await asyncio.wait_for(turn, 120))[0]
        await asyncio.sleep(0.5)
        rx.drain()
        draft = next((d for d in rx.drafts.values() if d.kind == "call"), None)
        rows = await memory.rows_of(root, conn.id, conversation_only=True)
        last = next((r for r in reversed(rows) if r.get("role") == "assistant"), {})
        content = str(last.get("content") or "")
        print(f"     stop={stopped} turn={out.get('status')} draft={draft and (len(draft.text), draft.ended, draft.aborted, draft.matched)}")
        print(f"     marker={content[:160]!r}")
        checks = [
            ("the draft reached rx before the stop", draft is not None and len(draft.text) >= 300),
            ("rx's stop was acknowledged", stopped == {"stopped": True}),
            ("the generation settled cancelled", out.get("status") == "cancelled"),
            ("the draft ABORTED (never ended, never matched)",
             draft is not None and draft.aborted == "stopped" and not draft.ended and not draft.matched),
            ("the turn keeps the marker, not the half answer", content.startswith("[stopped by rx]")),
            ("the marker names the call that was not sent", "[a send call to rx was not sent]" in content),
        ]
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "a receiver stopped the stream; nothing sent; the marker is on top"
                if allok else "a stream_stop check failed")
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
