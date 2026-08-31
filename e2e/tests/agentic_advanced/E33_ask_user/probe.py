"""agentic_advanced/ask_user — the model asks the HUMAN, and the answer steers it.

The ask protocol is prompt-taught: when the model cannot proceed without a human
decision it emits `{"type":"question", ...}` as strict JSON on its own line and
stops; the answer arrives as its next turn. Two arms, SAME task, different answers
— that difference is the measurement:

  explicit   — the human answers "crimson"; the stored value must be EXACTLY that.
               (Unguessable, so a model that skipped the ask cannot pass this arm.)
  judgement  — the human answers "do your best judgement" (the harness default);
               the model must pick a value ITSELF and still finish the task.

Scored on the STORE, never on prose. Both arms also require that a question was
actually asked (the harness's answer callback fired on a real, non-template ask).
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth.
sys.path.insert(
    0,
    str(
        next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
    ),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, verdict
from helpers.kernel import open_kernel
from helpers.proxy import DEFAULT_QUESTION_ANSWER

BK = _resolve_backend()

TASK = (
    "Store the user's favorite color in the memory agent `prefs`, under the key "
    "'color'. You do NOT know their favorite color and you must not guess it — "
    "ask the user (emit the question JSON on its own line, then stop; the answer "
    "arrives as your next message). If the answer names a color, store exactly "
    "that. If the answer leaves the choice to you, pick one yourself and store "
    "it. When the value is stored, you are done — stop."
)


async def _arm(name: str, answer: str):
    """One fresh kernel + AI, driven through ask → answer → act. Returns
    (questions_asked, stored_value, cap_reason).

    The AI id is UNIQUE PER ARM: ai_core keeps its FIFO lock / queue / provider
    maps keyed by bare agent id at module scope, so two in-process kernels that
    both name their AI "ai" share that state — arm 2's first send then queues
    behind arm 1's dead lock and the chain reads as ended with zero turns."""
    aid = f"ai_{name}"
    root, core, close_kernel = await open_kernel(
        f"ask_{name}_", reach=["prefs", "kernel_store", aid, "cli"]
    )
    store = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "file_bridge.tools",
                        "id": "kernel_store",
                        "root": ".",
                        "ingress_rule": "allow_all_ingress",
                        # Two-sided deny-all: the AI, and the wire surfaces the harness's own
                        # reads arrive as on a spawned kernel (inert in-process).
                        "callable_by": {aid: "*", "rest": "*", "web": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert not store.get("error"), f"store: {store}"
    made = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "durable_memory.tools",
                        "id": "prefs",
                        "file_bridge_id": store["id"],
                        "description": "The user's durable preferences.",
                        "callable_by": {aid: "*", "rest": "*", "web": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert not made.get("error"), f"prefs: {made}"

    asked: list[dict] = []

    def _answer(q: dict) -> str:
        asked.append(q)
        print(f"  [{name}] model asked: {q.get('question')!r} -> answering {answer!r}")
        return answer

    conn = await connect(
        root,
        core,
        BK,
        agent_id=aid,
        # `kernel_store` too: chat persistence rides `file_bridge_id`, and on a
        # spawned kernel that write goes through the gate like everything else.
        can_call={"prefs": "*", "cli": "*", "kernel_store": "*"},
        limits=Limits.from_env(os.environ),
        record={"file_bridge_id": store["id"]},
    )
    capped = await drive(conn, TASK, client_id=name, on_question=_answer)
    val = (
        (
            await root.send(
                [{"target": "prefs", "payload": {"type": "read", "key": "color"}}]
            )
        )[0]["result"]
        or {}
    ).get("value")
    await conn.close()
    await close_kernel()
    print(f"  [{name}] asked={len(asked)} stored={val!r} cap={capped or 'clean'}")
    return asked, val, capped


async def main():
    # Arm 1 — an EXPLICIT answer must be obeyed verbatim. "crimson" is not a
    # value a model guesses on its own, so passing this arm requires the ask.
    asked1, val1, _ = await _arm("explicit", "crimson")
    ok1 = bool(asked1) and val1 == "crimson"

    # Arm 2 — "do your best judgement" hands the decision back: the model must
    # choose SOME value itself and still finish.
    asked2, val2, _ = await _arm("judgement", DEFAULT_QUESTION_ANSWER)
    ok2 = bool(asked2) and isinstance(val2, str) and val2.strip() != ""

    print(f"\n  explicit : asked+stored-verbatim  -> {'OK' if ok1 else 'NO'}")
    print(f"  judgement: asked+chose-and-stored -> {'OK' if ok2 else 'NO'}")
    verdict(
        ok1 and ok2,
        "the ask protocol carries BOTH answer kinds"
        if ok1 and ok2
        else f"explicit={'ok' if ok1 else 'no'} judgement={'ok' if ok2 else 'no'}",
    )


if __name__ == "__main__":
    asyncio.run(main())
